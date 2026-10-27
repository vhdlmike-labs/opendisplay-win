//! H.264 encoding with Media Foundation (§5): BGRA desktop texture ->
//! D3D11 video processor (scale + BGRA->NV12, BT.709 limited range) ->
//! hardware encoder MFT (async, D3D-aware), or Microsoft's software encoder
//! when no hardware MFT works. Output is Annex B with 4-byte start codes and
//! SPS/PPS on every IDR, one access unit per call, no B-frames, IDRs on demand.

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use opendisplay_proto::video::{NalType, nal_units, normalize_start_codes};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoTaskMemFree, CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface};

use crate::capture::Gpu;

/// How many NV12 surfaces the converter rotates through, so the encoder can
/// still be reading one while the next is written.
const RING: usize = 4;

pub fn mf_startup() -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// BGRA -> NV12 on the GPU

pub struct Converter {
    vctx: ID3D11VideoContext,
    vdev: ID3D11VideoDevice,
    venum: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    input: Option<(usize, ID3D11VideoProcessorInputView)>,
}

impl Converter {
    pub fn new(gpu: &Gpu, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Result<Self> {
        unsafe {
            let vdev: ID3D11VideoDevice = gpu.device.cast()?;
            let vctx: ID3D11VideoContext = gpu.context.cast()?;
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: DXGI_RATIONAL { Numerator: fps, Denominator: 1 },
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let venum = vdev.CreateVideoProcessorEnumerator(&content)?;
            let vp = vdev.CreateVideoProcessor(&venum, 0)?;

            // Full-range RGB in, BT.709 limited-range YCbCr out (what the
            // encoder's VUI and the receivers' decoders assume).
            let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 }; // Usage 0, RGB full range
            // YCbCr_Matrix (bit 2) = 1 -> BT.709; Nominal_Range (bits 4-5) = 1 -> 16-235.
            let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
            vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &in_cs);
            vctx.VideoProcessorSetOutputColorSpace(&vp, &out_cs);
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            let full_in = RECT { left: 0, top: 0, right: in_w as i32, bottom: in_h as i32 };
            let full_out = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
            vctx.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&full_in));
            vctx.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&full_out));
            vctx.VideoProcessorSetOutputTargetRect(&vp, true, Some(&full_out));

            let desc = D3D11_TEXTURE2D_DESC {
                Width: out_w,
                Height: out_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut outputs = Vec::new();
            for _ in 0..RING {
                let mut tex = None;
                if gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)).is_err() {
                    // Some drivers refuse BIND_VIDEO_ENCODER on plain textures.
                    let d = D3D11_TEXTURE2D_DESC { BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32, ..desc };
                    gpu.device.CreateTexture2D(&d, None, Some(&mut tex))?;
                }
                let tex = tex.context("no NV12 texture")?;
                let vdesc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut view = None;
                vdev.CreateVideoProcessorOutputView(&tex, &venum, &vdesc, Some(&mut view))?;
                outputs.push((tex, view.context("no output view")?));
            }
            Ok(Self { vctx, vdev, venum, vp, outputs, next: 0, input: None })
        }
    }

    /// Convert `src` into the next ring surface and return it.
    pub fn convert(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        unsafe {
            let key = src.as_raw() as usize;
            if self.input.as_ref().map(|(k, _)| *k) != Some(key) {
                let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                    },
                };
                let mut view = None;
                self.vdev
                    .CreateVideoProcessorInputView(src, &self.venum, &desc, Some(&mut view))?;
                self.input = Some((key, view.context("no input view")?));
            }
            let (_, in_view) = self.input.as_ref().unwrap();
            let (tex, out_view) = &self.outputs[self.next];
            self.next = (self.next + 1) % RING;
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: TRUE,
                pInputSurface: ManuallyDrop::new(Some(in_view.clone())),
                ..Default::default()
            };
            let streams = [stream];
            let r = self.vctx.VideoProcessorBlt(&self.vp, out_view, 0, &streams);
            let [mut stream] = streams;
            ManuallyDrop::drop(&mut stream.pInputSurface);
            r.context("VideoProcessorBlt")?;
            Ok(tex.clone())
        }
    }
}

// ---------------------------------------------------------------------------
// Media Foundation encoder

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub prefer_software: bool,
}

pub struct Encoded {
    pub annexb: Vec<u8>,
    pub idr: bool,
}

pub struct Encoder {
    pub name: String,
    pub hardware: bool,
    mft: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    _manager: Option<IMFDXGIDeviceManager>,
    /// Software path: NV12 staging texture to read frames back.
    staging: Option<ID3D11Texture2D>,
    context: ID3D11DeviceContext,
    need_input: u32,
    provides_samples: bool,
    out_size: u32,
    cfg_w: u32,
    cfg_h: u32,
    fps: u32,
    frame_index: u64,
    sequence_header: Vec<u8>,
}

fn set_codec(codec: &ICodecAPI, api: &GUID, v: VARIANT, what: &str) {
    if let Err(e) = unsafe { codec.SetValue(api, &v) } {
        debug!("encoder ignores {what}: {e}");
    }
}

fn media_type(sub: &GUID, cfg: &EncoderConfig) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, sub)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, ((cfg.width as u64) << 32) | cfg.height as u64)?;
        t.SetUINT64(&MF_MT_FRAME_RATE, ((cfg.fps as u64) << 32) | 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        if *sub == MFVideoFormat_H264 {
            t.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)?;
            t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        }
        Ok(t)
    }
}

/// Hardware H.264 encoder MFTs, best first, as (name, activate).
fn hardware_encoders() -> Vec<(String, IMFActivate)> {
    let mut out = Vec::new();
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut arr,
            &mut count,
        )
        .is_err()
        {
            return out;
        }
        for i in 0..count as usize {
            if let Some(a) = (*arr.add(i)).take() {
                let mut name = String::from("hardware encoder");
                let mut p = windows::core::PWSTR::null();
                let mut len = 0;
                if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() {
                    name = p.to_string().unwrap_or(name);
                    CoTaskMemFree(Some(p.0 as _));
                }
                out.push((name, a));
            }
        }
        CoTaskMemFree(Some(arr as _));
    }
    out
}

impl Encoder {
    pub fn new(gpu: &Gpu, cfg: &EncoderConfig) -> Result<Self> {
        if !cfg.prefer_software {
            for (name, activate) in hardware_encoders() {
                match Self::new_hardware(gpu, cfg, &name, &activate) {
                    Ok(e) => return Ok(e),
                    Err(e) => {
                        warn!("{name}: {e:#}; trying the next encoder");
                        unsafe { let _ = activate.ShutdownObject(); }
                    }
                }
            }
        }
        Self::new_software(gpu, cfg)
    }

    fn configure(mft: &IMFTransform, cfg: &EncoderConfig) -> Result<Option<ICodecAPI>> {
        unsafe {
            let codec = mft.cast::<ICodecAPI>().ok();
            if let Some(c) = &codec {
                set_codec(c, &CODECAPI_AVLowLatencyMode, VARIANT::from(true), "low latency");
                set_codec(c, &CODECAPI_AVEncCommonRateControlMode,
                    VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32), "CBR");
                set_codec(c, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(cfg.bitrate), "bitrate");
                set_codec(c, &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32), "no B-frames");
                // Periodic keyframes off: IDRs only on demand (§5.3).
                set_codec(c, &CODECAPI_AVEncMPVGOPSize, VARIANT::from(cfg.fps * 3600), "GOP");
                set_codec(c, &CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(60u32), "quality/speed");
            }
            // Output type first, then input (encoder MFT rule).
            mft.SetOutputType(0, &media_type(&MFVideoFormat_H264, cfg)?, 0)
                .context("SetOutputType H.264")?;
            mft.SetInputType(0, &media_type(&MFVideoFormat_NV12, cfg)?, 0)
                .context("SetInputType NV12")?;
            Ok(codec)
        }
    }

    fn new_hardware(gpu: &Gpu, cfg: &EncoderConfig, name: &str, activate: &IMFActivate) -> Result<Self> {
        unsafe {
            let mft: IMFTransform = activate.ActivateObject()?;
            let attrs = mft.GetAttributes()?;
            let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            if is_async {
                attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            }
            if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                bail!("not D3D11 aware");
            }
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.context("no device manager")?;
            manager.ResetDevice(&gpu.device, token)?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("SET_D3D_MANAGER")?;
            let codec = Self::configure(&mft, cfg)?;
            let events = if is_async { Some(mft.cast::<IMFMediaEventGenerator>()?) } else { None };
            let mut enc = Self::finish(mft, gpu, cfg, name.to_string(), true, events, codec, Some(manager))?;
            enc.start()?;
            Ok(enc)
        }
    }

    fn new_software(gpu: &Gpu, cfg: &EncoderConfig) -> Result<Self> {
        unsafe {
            let mft: IMFTransform = windows::Win32::System::Com::CoCreateInstance(
                &CLSID_MSH264EncoderMFT,
                None,
                windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
            )
            .context("creating the Microsoft H.264 software encoder")?;
            let codec = Self::configure(&mft, cfg)?;
            let mut enc = Self::finish(mft, gpu, cfg, "Microsoft H.264 software encoder".into(), false, None, codec, None)?;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: cfg.width,
                Height: cfg.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            gpu.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
            enc.staging = staging;
            enc.start()?;
            Ok(enc)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(
        mft: IMFTransform,
        gpu: &Gpu,
        cfg: &EncoderConfig,
        name: String,
        hardware: bool,
        events: Option<IMFMediaEventGenerator>,
        codec: Option<ICodecAPI>,
        manager: Option<IMFDXGIDeviceManager>,
    ) -> Result<Self> {
        let info = unsafe { mft.GetOutputStreamInfo(0)? };
        let provides_samples = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32
            != 0;
        let mut enc = Self {
            name,
            hardware,
            mft,
            events,
            codec,
            _manager: manager,
            staging: None,
            context: gpu.context.clone(),
            need_input: 0,
            provides_samples,
            out_size: info.cbSize.max(cfg.width * cfg.height * 3 / 2),
            cfg_w: cfg.width,
            cfg_h: cfg.height,
            fps: cfg.fps,
            frame_index: 0,
            sequence_header: Vec::new(),
        };
        enc.refresh_sequence_header();
        Ok(enc)
    }

    fn start(&mut self) -> Result<()> {
        unsafe {
            // Some encoders (the Microsoft software MFT) reject a flush before streaming.
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        info!(
            "encoder: {} ({}, {}x{} @{})",
            self.name,
            if self.hardware { "GPU" } else { "CPU" },
            self.cfg_w,
            self.cfg_h,
            self.fps
        );
        Ok(())
    }

    fn refresh_sequence_header(&mut self) {
        unsafe {
            if let Ok(t) = self.mft.GetOutputCurrentType(0) {
                if let Ok(size) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                    let mut buf = vec![0u8; size as usize];
                    if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None).is_ok() {
                        self.sequence_header = normalize_start_codes(&buf);
                    }
                }
            }
        }
    }

    fn make_input(&mut self, nv12: &ID3D11Texture2D) -> Result<IMFSample> {
        unsafe {
            let buffer = if let Some(staging) = &self.staging {
                // Software encoder: read the NV12 surface back into system memory.
                self.context.CopyResource(staging, nv12);
                let mut map = D3D11_MAPPED_SUBRESOURCE::default();
                self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
                let (w, h) = (self.cfg_w as usize, self.cfg_h as usize);
                let len = w * h * 3 / 2;
                let buffer = MFCreateMemoryBuffer(len as u32)?;
                let mut dst = std::ptr::null_mut();
                buffer.Lock(&mut dst, None, None)?;
                let src = map.pData as *const u8;
                let pitch = map.RowPitch as usize;
                for row in 0..h + h / 2 {
                    std::ptr::copy_nonoverlapping(src.add(row * pitch), dst.add(row * w), w);
                }
                buffer.Unlock()?;
                buffer.SetCurrentLength(len as u32)?;
                self.context.Unmap(staging, 0);
                buffer
            } else {
                let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)?;
                if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                    buffer.SetCurrentLength(b2.GetContiguousLength()?)?;
                }
                buffer
            };
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            let duration = 10_000_000 / self.fps as i64;
            sample.SetSampleTime(self.frame_index as i64 * duration)?;
            sample.SetSampleDuration(duration)?;
            self.frame_index += 1;
            Ok(sample)
        }
    }

    /// Encode one NV12 picture; returns the access unit(s) the encoder
    /// produced for it (normally exactly one).
    pub fn encode(&mut self, nv12: &ID3D11Texture2D, force_idr: bool) -> Result<Vec<Encoded>> {
        if force_idr {
            if let Some(c) = &self.codec {
                set_codec(c, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32), "force keyframe");
            }
        }
        let sample = self.make_input(nv12)?;
        if force_idr {
            unsafe { let _ = sample.SetUINT32(&MFSampleExtension_VideoEncodePictureType, 2); }
        }
        let mut out = Vec::new();
        if self.events.is_none() {
            unsafe { self.mft.ProcessInput(0, &sample, 0).context("ProcessInput")? };
            self.drain_sync(&mut out)?;
            return Ok(out);
        }

        // Async MFT: wait for an input slot, submit, then wait for the output.
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut submitted = false;
        loop {
            if submitted && !out.is_empty() {
                // Collect anything else already queued, without waiting.
                self.pump_events(false, &mut out)?;
                return Ok(out);
            }
            if !submitted && self.need_input > 0 {
                unsafe { self.mft.ProcessInput(0, &sample, 0).context("ProcessInput")? };
                self.need_input -= 1;
                submitted = true;
                continue;
            }
            if Instant::now() > deadline {
                if submitted {
                    return Ok(out); // arrives with the next frame
                }
                bail!("encoder never asked for input");
            }
            if !self.pump_events(true, &mut out)? {
                std::thread::sleep(Duration::from_micros(500));
            }
        }
    }

    /// Handle queued async events; returns whether any were handled.
    fn pump_events(&mut self, stop_on_output: bool, out: &mut Vec<Encoded>) -> Result<bool> {
        let events = self.events.clone().unwrap();
        let mut any = false;
        loop {
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(any),
                Err(e) => return Err(e).context("GetEvent"),
            };
            any = true;
            let ty = unsafe { event.GetType()? } as i32;
            if ty == METransformNeedInput.0 {
                self.need_input += 1;
            } else if ty == METransformHaveOutput.0 {
                if let Some(e) = self.process_output()? {
                    out.push(e);
                }
                if stop_on_output {
                    return Ok(true);
                }
            } else if ty == MEError.0 {
                let status = unsafe { event.GetStatus()? };
                bail!("encoder error event: {status:?}");
            }
        }
    }

    fn drain_sync(&mut self, out: &mut Vec<Encoded>) -> Result<()> {
        loop {
            match self.process_output() {
                Ok(Some(e)) => out.push(e),
                Ok(None) => continue, // stream change handled; try again
                Err(e) => {
                    let need_more = e
                        .downcast_ref::<windows::core::Error>()
                        .is_some_and(|w| w.code() == MF_E_TRANSFORM_NEED_MORE_INPUT);
                    return if need_more { Ok(()) } else { Err(e) };
                }
            }
        }
    }

    /// One ProcessOutput call. `Ok(None)` after a format renegotiation.
    fn process_output(&mut self) -> Result<Option<Encoded>> {
        unsafe {
            let sample = if self.provides_samples {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                Some(s)
            };
            let mut buf = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status);
            let sample = ManuallyDrop::take(&mut buf.pSample);
            drop(ManuallyDrop::take(&mut buf.pEvents));
            if let Err(e) = r {
                if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    self.refresh_sequence_header();
                    return Ok(None);
                }
                return Err(anyhow!(e));
            }
            let sample = sample.context("encoder produced no sample")?;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let raw = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            Ok(Some(self.to_access_unit(&raw)))
        }
    }

    /// Normalize to 4-byte start codes and make sure an IDR carries SPS/PPS (§5.1).
    fn to_access_unit(&mut self, raw: &[u8]) -> Encoded {
        let mut annexb = normalize_start_codes(raw);
        let (mut idr, mut sps) = (false, false);
        for nal in nal_units(&annexb) {
            match NalType::of(nal) {
                Some(NalType::IdrSlice) => idr = true,
                Some(NalType::Sps) => sps = true,
                _ => {}
            }
        }
        if sps && idr {
            // Remember the encoder's current parameter sets.
            let mut header = Vec::new();
            for nal in nal_units(&annexb) {
                if matches!(NalType::of(nal), Some(NalType::Sps | NalType::Pps)) {
                    header.extend_from_slice(&[0, 0, 0, 1]);
                    header.extend_from_slice(nal);
                }
            }
            self.sequence_header = header;
        } else if idr && !sps {
            if self.sequence_header.is_empty() {
                self.refresh_sequence_header();
            }
            let mut with = self.sequence_header.clone();
            with.extend_from_slice(&annexb);
            annexb = with;
        }
        Encoded { annexb, idr }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            if let Some(e) = &self.events {
                if let Ok(s) = e.cast::<IMFShutdown>() {
                    let _ = s.Shutdown();
                }
            }
        }
    }
}
