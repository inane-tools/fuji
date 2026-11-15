//! Video playback, for the viewer.
//!
//! On Windows, Media Foundation's Media Engine plays the video straight from
//! iCloud's URL (sound included) and hands over each new frame, which is
//! copied into an image GPUI can draw. Elsewhere there's no player yet and
//! the viewer offers a download instead.

use std::sync::Arc;

use gpui::RenderImage;

/// A playing (or paused) video.
pub struct VideoPlayer {
    #[cfg(windows)]
    inner: imp::Player,
}

impl VideoPlayer {
    /// Start playing `url` (paused if `autoplay` is off).
    #[cfg(windows)]
    pub fn open(url: &str, autoplay: bool) -> Result<Self, String> {
        Ok(Self {
            inner: imp::Player::open(url, autoplay).map_err(|e| e.message())?,
        })
    }

    #[cfg(not(windows))]
    pub fn open(_url: &str, _autoplay: bool) -> Result<Self, String> {
        Err("Video playback isn't available on this system yet.".into())
    }

    /// The newest frame, if one has arrived since the last call.
    pub fn next_frame(&mut self) -> Option<Arc<RenderImage>> {
        #[cfg(windows)]
        return self.inner.next_frame();
        #[cfg(not(windows))]
        None
    }

    pub fn toggle(&self) {
        #[cfg(windows)]
        self.inner.toggle();
    }

    pub fn is_paused(&self) -> bool {
        #[cfg(windows)]
        return self.inner.is_paused();
        #[cfg(not(windows))]
        true
    }

    /// Seconds played and in all (0 until known).
    pub fn position(&self) -> (f64, f64) {
        #[cfg(windows)]
        return self.inner.position();
        #[cfg(not(windows))]
        (0., 0.)
    }

    pub fn seek(&self, seconds: f64) {
        #[cfg(windows)]
        self.inner.seek(seconds);
        #[cfg(not(windows))]
        let _ = seconds;
    }

    pub fn is_muted(&self) -> bool {
        #[cfg(windows)]
        return self.inner.is_muted();
        #[cfg(not(windows))]
        false
    }

    pub fn set_muted(&self, muted: bool) {
        #[cfg(windows)]
        self.inner.set_muted(muted);
        #[cfg(not(windows))]
        let _ = muted;
    }

    /// Why it can't play, if it can't.
    pub fn error(&self) -> Option<String> {
        #[cfg(windows)]
        return self.inner.error();
        #[cfg(not(windows))]
        None
    }
}

/// "1:05", "12:00", "1:02:03".
pub fn format_time(seconds: f64) -> String {
    let total = if seconds.is_finite() {
        seconds.max(0.).floor() as u64
    } else {
        0
    };
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, OnceLock};

    use gpui::RenderImage;
    use smallvec::smallvec;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICBitmap, IWICImagingFactory,
        WICBitmapCacheOnDemand, WICBitmapLockRead, WICRect,
    };
    use windows::Win32::Media::MediaFoundation::{
        CLSID_MFMediaEngineClassFactory, IMFAttributes, IMFMediaEngine, IMFMediaEngineClassFactory,
        IMFMediaEngineNotify, IMFMediaEngineNotify_Impl, MF_MEDIA_ENGINE_CALLBACK,
        MF_MEDIA_ENGINE_EVENT_ERROR, MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, MF_VERSION,
        MFCreateAttributes, MFSTARTUP_FULL, MFStartup,
    };
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::core::{BSTR, Result, implement};

    /// What the engine reports from its own threads.
    #[derive(Default)]
    struct Events {
        error: AtomicBool,
        error_code: AtomicU32,
    }

    #[implement(IMFMediaEngineNotify)]
    struct Notify(Arc<Events>);

    impl IMFMediaEngineNotify_Impl for Notify_Impl {
        fn EventNotify(&self, event: u32, param1: usize, _param2: u32) -> Result<()> {
            if event == MF_MEDIA_ENGINE_EVENT_ERROR.0 as u32 {
                self.0.error_code.store(param1 as u32, Ordering::Relaxed);
                self.0.error.store(true, Ordering::Relaxed);
            }
            Ok(())
        }
    }

    pub struct Player {
        engine: IMFMediaEngine,
        events: Arc<Events>,
        wic: IWICImagingFactory,
        /// Where frames land, at the video's size.
        canvas: Option<(IWICBitmap, u32, u32)>,
        last_pts: Option<i64>,
    }

    impl Player {
        pub fn open(url: &str, autoplay: bool) -> Result<Self> {
            static STARTED: OnceLock<bool> = OnceLock::new();
            // SAFETY: plain Media Foundation and COM calls with valid
            // arguments; the objects live as long as the player.
            unsafe {
                if !*STARTED.get_or_init(|| MFStartup(MF_VERSION, MFSTARTUP_FULL).is_ok()) {
                    return Err(windows::core::Error::from_win32());
                }
                let factory: IMFMediaEngineClassFactory =
                    CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)?;
                let mut attributes: Option<IMFAttributes> = None;
                MFCreateAttributes(&mut attributes, 2)?;
                let attributes = attributes.ok_or_else(windows::core::Error::empty)?;
                let events = Arc::new(Events::default());
                let notify: IMFMediaEngineNotify = Notify(events.clone()).into();
                attributes.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify)?;
                attributes.SetUINT32(
                    &MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT,
                    DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32,
                )?;
                // No window: frames are asked for (frame-server mode).
                let engine = factory.CreateInstance(0, &attributes)?;
                engine.SetAutoPlay(autoplay)?;
                engine.SetSource(&BSTR::from(url))?;
                let wic: IWICImagingFactory =
                    CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
                Ok(Self {
                    engine,
                    events,
                    wic,
                    canvas: None,
                    last_pts: None,
                })
            }
        }

        pub fn next_frame(&mut self) -> Option<Arc<RenderImage>> {
            // SAFETY: as above; the bitmap is locked only while copied.
            unsafe {
                let pts = self.engine.OnVideoStreamTick().ok()?;
                if self.last_pts == Some(pts) {
                    return None;
                }
                let (mut width, mut height) = (0u32, 0u32);
                self.engine
                    .GetNativeVideoSize(Some(&mut width), Some(&mut height))
                    .ok()?;
                if width == 0 || height == 0 {
                    return None;
                }
                if !matches!(&self.canvas, Some((_, w, h)) if *w == width && *h == height) {
                    let bitmap = self
                        .wic
                        .CreateBitmap(
                            width,
                            height,
                            &GUID_WICPixelFormat32bppBGRA,
                            WICBitmapCacheOnDemand,
                        )
                        .ok()?;
                    self.canvas = Some((bitmap, width, height));
                }
                let (bitmap, _, _) = self.canvas.as_ref()?;
                let rect = RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                };
                self.engine
                    .TransferVideoFrame(bitmap, None, &rect, None)
                    .ok()?;
                self.last_pts = Some(pts);

                let lock = bitmap
                    .Lock(
                        &WICRect {
                            X: 0,
                            Y: 0,
                            Width: width as i32,
                            Height: height as i32,
                        },
                        WICBitmapLockRead.0 as u32,
                    )
                    .ok()?;
                let stride = lock.GetStride().ok()? as usize;
                let mut size = 0u32;
                let mut data = std::ptr::null_mut();
                lock.GetDataPointer(&mut size, &mut data).ok()?;
                let source = std::slice::from_raw_parts(data, size as usize);
                let row = width as usize * 4;
                let mut pixels = Vec::with_capacity(row * height as usize);
                for y in 0..height as usize {
                    pixels.extend_from_slice(source.get(y * stride..y * stride + row)?);
                }
                drop(lock);
                // Video has no transparency, whatever the alpha says.
                for alpha in pixels.iter_mut().skip(3).step_by(4) {
                    *alpha = 255;
                }
                // GPUI keeps images as BGRA, which this already is.
                let buffer = image::RgbaImage::from_raw(width, height, pixels)?;
                Some(Arc::new(RenderImage::new(smallvec![image::Frame::new(
                    buffer
                )])))
            }
        }

        pub fn toggle(&self) {
            // SAFETY: calls on a live engine.
            unsafe {
                if self.engine.IsEnded().as_bool() {
                    let _ = self.engine.SetCurrentTime(0.);
                    let _ = self.engine.Play();
                } else if self.engine.IsPaused().as_bool() {
                    let _ = self.engine.Play();
                } else {
                    let _ = self.engine.Pause();
                }
            }
        }

        pub fn is_paused(&self) -> bool {
            // SAFETY: as above.
            unsafe { self.engine.IsPaused().as_bool() || self.engine.IsEnded().as_bool() }
        }

        pub fn position(&self) -> (f64, f64) {
            // SAFETY: as above.
            unsafe {
                let duration = self.engine.GetDuration();
                (
                    self.engine.GetCurrentTime(),
                    if duration.is_finite() { duration } else { 0. },
                )
            }
        }

        pub fn seek(&self, seconds: f64) {
            // SAFETY: as above.
            unsafe {
                let _ = self.engine.SetCurrentTime(seconds.max(0.));
            }
        }

        pub fn is_muted(&self) -> bool {
            // SAFETY: as above.
            unsafe { self.engine.GetMuted().as_bool() }
        }

        pub fn set_muted(&self, muted: bool) {
            // SAFETY: as above.
            unsafe {
                let _ = self.engine.SetMuted(muted);
            }
        }

        pub fn error(&self) -> Option<String> {
            self.events.error.load(Ordering::Relaxed).then(|| {
                match self.events.error_code.load(Ordering::Relaxed) {
                    // MF_MEDIA_ENGINE_ERR_SRC_NOT_SUPPORTED
                    4 => "This video's format can't be played here. Download it to watch."
                        .to_string(),
                    // MF_MEDIA_ENGINE_ERR_NETWORK
                    2 => "The video couldn't be loaded.".to_string(),
                    _ => "The video couldn't be played.".to_string(),
                }
            })
        }
    }

    impl Drop for Player {
        fn drop(&mut self) {
            // SAFETY: shutting down a live engine once.
            unsafe {
                let _ = self.engine.Shutdown();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_like_a_player() {
        assert_eq!(format_time(0.), "0:00");
        assert_eq!(format_time(65.4), "1:05");
        assert_eq!(format_time(3723.), "1:02:03");
        assert_eq!(format_time(f64::NAN), "0:00");
    }
}
