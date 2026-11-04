use super::*;

/// Read-only diagnostic: compare the actual Windows roles with CPAL's default,
/// never substitute the first enumerated device or modify Windows settings.
#[test]
#[ignore = "reads current Windows audio endpoint defaults"]
fn real_defaults_match_windows_console_role_not_device_order() {
    use windows::Win32::{
        Media::Audio::{
            IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eCommunications, eConsole,
            eMultimedia, eRender,
        },
        System::Com::{
            CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
            CoUninitialize,
        },
    };
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.unwrap();
    let inventory = devices().unwrap();
    let host = cpal::default_host();
    for (source, flow) in [(0, eCapture), (1, eRender)] {
        for (role, label) in [
            (eConsole, "console"),
            (eMultimedia, "multimedia"),
            (eCommunications, "communications"),
        ] {
            let Ok(endpoint) = (unsafe { enumerator.GetDefaultAudioEndpoint(flow, role) }) else {
                continue;
            };
            let raw = unsafe { endpoint.GetId() }.unwrap();
            let id = unsafe { raw.to_string() }.unwrap();
            unsafe { CoTaskMemFree(Some(raw.0.cast())) };
            let canonical = format!("wasapi:{id}");
            let name = inventory
                .iter()
                .find(|d| d.id == canonical)
                .map(|d| d.name.as_str())
                .unwrap_or("unlisted");
            tracing::info!(source, role = label, device = name, "Windows 默认音频设备");
            if role == eConsole {
                assert_eq!(
                    default_device(&host, source)
                        .unwrap()
                        .id()
                        .unwrap()
                        .to_string(),
                    canonical
                );
                assert_eq!(
                    inventory
                        .iter()
                        .filter(|d| d.source == source && d.is_default)
                        .count(),
                    1,
                    "exactly one default per available source"
                );
                if source == 0 {
                    let listed = super::super::list_input_devices();
                    let capture_default = host.default_input_device().unwrap();
                    assert_eq!(capture_default.id().unwrap().to_string(), canonical);
                    assert_eq!(
                        listed
                            .iter()
                            .filter(|d| d.is_default)
                            .map(|d| d.name.as_str())
                            .collect::<Vec<_>>(),
                        vec![capture_default.to_string().as_str()],
                        "voice settings and recording must agree on the default input"
                    );
                }
            }
        }
    }
    drop(enumerator);
    if initialized.is_ok() {
        unsafe { CoUninitialize() };
    }
}

/// Opt-in hardware check: plays a quiet synthetic tone, captures WASAPI loopback,
/// and verifies both preview meters and saved PCM. Never stores microphone input.
#[test]
#[ignore = "requires a Windows playback device and briefly plays a quiet test tone"]
fn real_loopback_has_energy_in_preview_and_saved_pcm() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
    let host = cpal::default_host();
    let Some(output) = host.default_output_device() else {
        return;
    };
    let config = output.default_output_config().unwrap();
    assert_eq!(
        config.sample_format(),
        cpal::SampleFormat::F32,
        "fixture requires float output"
    );
    let rate = config.sample_rate();
    let channels = config.channels() as usize;
    let mut frame = 0u64;
    let tone = output
        .build_output_stream(
            config.into(),
            move |data: &mut [f32], _| {
                for values in data.chunks_mut(channels) {
                    let value = (frame as f64 * 440.0 * std::f64::consts::TAU / rate as f64).sin()
                        as f32
                        * 0.08;
                    values.fill(value);
                    frame += 1;
                }
            },
            |_| {},
            None,
        )
        .unwrap();
    tone.play().unwrap();
    for persist in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let shared = crate::gif::audio::shared(AudioOptions {
            mode: AudioMode::System,
            ..Default::default()
        });
        let mut capture = Session::new(shared.clone(), persist.then(|| temp.path().to_path_buf()));
        let start = capture.resume(0).unwrap();
        let mut peak = 0.0f32;
        while start.elapsed() < Duration::from_millis(700) {
            std::thread::sleep(Duration::from_millis(50));
            let status = shared.lock().unwrap().snapshot();
            assert!(status.error.is_none(), "capture fault: {:?}", status.error);
            peak = peak.max(status.peak[1]);
        }
        capture.pause(start.elapsed().as_millis() as u64);
        assert!(
            peak > 0.001,
            "loopback meter is silent (persist={persist}, peak={peak})"
        );
        if persist {
            let pcm = std::fs::read(track_path(temp.path(), 1)).unwrap();
            let energy = pcm
                .chunks_exact(2)
                .map(|v| i16::from_le_bytes([v[0], v[1]]).unsigned_abs())
                .max()
                .unwrap_or(0);
            assert!(energy > 32, "saved loopback is silent (peak={energy})");
            let audio = manifest(temp.path()).unwrap().unwrap();
            assert!(audio.complete && audio.committed_ms >= 600);
        }
    }
}

#[test]
#[ignore = "requires a Windows microphone; checks callback delivery without saving audio"]
fn real_microphone_delivers_live_meter_samples_and_releases_device() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
    if cpal::default_host().default_input_device().is_none() {
        return;
    }
    let shared = crate::gif::audio::shared(AudioOptions {
        mode: AudioMode::Microphone,
        ..Default::default()
    });
    let mut capture = Session::new(shared.clone(), None);
    let start = capture.resume(0).unwrap();
    while start.elapsed() < Duration::from_millis(600) {
        std::thread::sleep(Duration::from_millis(25));
    }
    {
        let mut gate = shared.lock().unwrap();
        let status = gate.snapshot();
        assert!(
            status.error.is_none(),
            "microphone fault: {:?}",
            status.error
        );
        assert!(
            gate.meter_updated[0].is_some(),
            "microphone callback did not reach the meter"
        );
        assert!(status.rms[0].is_finite() && status.peak[0].is_finite());
        assert_eq!(status.tracks[0].source, 0);
    }
    capture.pause(start.elapsed().as_millis() as u64);
    drop(capture);
    assert!(super::super::MicrophoneLease::acquire().is_ok());
}
