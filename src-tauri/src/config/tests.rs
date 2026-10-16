#[cfg(test)]
mod tests {
    use crate::config::*;

    #[test]
    fn retrigger_defaults_to_queue_and_round_trips_both_modes() {
        assert_eq!(
            AppConfig::default().playback.on_retrigger,
            RetriggerMode::Queue,
            "an interrupted reading must be opt-in"
        );
        for (wire, expected) in [
            ("interrupt", RetriggerMode::Interrupt),
            ("queue", RetriggerMode::Queue),
        ] {
            let raw = format!(r#"{{"on_retrigger":"{wire}"}}"#);
            let parsed: PlaybackConfig = serde_json::from_str(&raw).unwrap();
            assert_eq!(parsed.on_retrigger, expected);
            assert_eq!(
                serde_json::to_value(&parsed).unwrap()["on_retrigger"],
                serde_json::json!(wire)
            );
        }
    }

    #[test]
    fn streaming_setting_defaults_on_and_persists_when_disabled() {
        let mut config = serde_json::to_value(AppConfig::default()).unwrap();
        assert_eq!(config["playback"]["streaming_enabled"], true);
        config["playback"]
            .as_object_mut()
            .unwrap()
            .remove("streaming_enabled");
        let mut restored: AppConfig = serde_json::from_value(config).unwrap();
        assert!(restored.playback.streaming_enabled);
        restored.playback.streaming_enabled = false;
        let saved = serde_json::to_string(&restored).unwrap();
        let restored: AppConfig = serde_json::from_str(&saved).unwrap();
        assert!(!restored.playback.streaming_enabled);
    }

    #[test]
    fn test_expand_filename_pattern_timestamp() {
        let result = expand_filename_pattern("output_{timestamp}.wav", "af_heart", "hello");
        assert!(result.starts_with("output_"));
        assert!(result.ends_with(".wav"));
        let ts_part = result
            .strip_prefix("output_")
            .unwrap()
            .strip_suffix(".wav")
            .unwrap();
        assert!(ts_part.parse::<i64>().is_ok());
    }

    #[test]
    fn test_expand_filename_pattern_voice() {
        let result = expand_filename_pattern("tts_{voice}.wav", "af_heart", "hello");
        assert_eq!(result, "tts_af_heart.wav");
    }

    #[test]
    fn test_expand_filename_pattern_text_sanitization() {
        let result = expand_filename_pattern("{text}.wav", "voice", "Hello World");
        assert_eq!(result, "hello_world.wav");

        let result = expand_filename_pattern("{text}.wav", "voice", "Hello! @World#");
        assert_eq!(result, "hello_world.wav");

        let long_text = "This is a very long text that should be truncated at thirty characters";
        let result = expand_filename_pattern("{text}.wav", "voice", long_text);
        let text_part = result.strip_suffix(".wav").unwrap();
        assert!(
            text_part.len() <= 30,
            "Text part '{}' is {} chars",
            text_part,
            text_part.len()
        );
    }

    #[test]
    fn test_expand_filename_pattern_datetime() {
        let result = expand_filename_pattern("copyspeak_{datetime}.wav", "voice", "text");
        assert!(result.starts_with("copyspeak_"));
        assert!(result.ends_with(".wav"));
        let datetime_part = result
            .strip_prefix("copyspeak_")
            .unwrap()
            .strip_suffix(".wav")
            .unwrap();
        assert_eq!(datetime_part.len(), 17); // YYYY-MM-DD_HHMMSS
        assert!(datetime_part.chars().nth(4) == Some('-'));
        assert!(datetime_part.chars().nth(7) == Some('-'));
        assert!(datetime_part.chars().nth(10) == Some('_'));
    }

    #[test]
    fn test_expand_filename_pattern_date_and_time() {
        let result = expand_filename_pattern("{date}_{time}.wav", "voice", "text");
        assert!(result.ends_with(".wav"));
        let parts: Vec<&str> = result.strip_suffix(".wav").unwrap().split('_').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), 10); // YYYY-MM-DD
        assert_eq!(parts[1].len(), 6); // HHMMSS
    }

    #[test]
    fn test_expand_filename_pattern_multiple_placeholders() {
        let result = expand_filename_pattern("{voice}_{text}_{date}.wav", "af_heart", "Hello");
        assert!(result.starts_with("af_heart_hello_"));
        assert!(result.ends_with(".wav"));
    }

    #[test]
    fn test_expand_filename_pattern_no_placeholders() {
        let result = expand_filename_pattern("static_filename.wav", "voice", "text");
        assert_eq!(result, "static_filename.wav");
    }

    #[test]
    fn test_expand_filename_pattern_voice_sanitization() {
        let result = expand_filename_pattern("{voice}.wav", "voice/with:bad<chars>", "text");
        assert_eq!(result, "voicewithbadchars.wav");
    }

    // ========== AudioFormat Tests ==========

    #[test]
    fn test_audio_format_default_extension() {
        assert_eq!(AudioFormat::Wav.default_extension(), "wav");
        assert_eq!(AudioFormat::Mp3.default_extension(), "mp3");
        assert_eq!(AudioFormat::Ogg.default_extension(), "ogg");
        assert_eq!(AudioFormat::Flac.default_extension(), "flac");
    }

    #[test]
    fn test_audio_format_from_extension() {
        assert_eq!(AudioFormat::from_extension("wav"), Some(AudioFormat::Wav));
        assert_eq!(AudioFormat::from_extension("WAV"), Some(AudioFormat::Wav));
        assert_eq!(AudioFormat::from_extension("mp3"), Some(AudioFormat::Mp3));
        assert_eq!(AudioFormat::from_extension("MP3"), Some(AudioFormat::Mp3));
        assert_eq!(AudioFormat::from_extension("ogg"), Some(AudioFormat::Ogg));
        assert_eq!(AudioFormat::from_extension("flac"), Some(AudioFormat::Flac));
        assert_eq!(AudioFormat::from_extension("xyz"), None);
    }

    #[test]
    fn test_audio_format_serialization() {
        assert_eq!(serde_json::to_string(&AudioFormat::Wav).unwrap(), "\"wav\"");
        assert_eq!(serde_json::to_string(&AudioFormat::Mp3).unwrap(), "\"mp3\"");
        assert_eq!(serde_json::to_string(&AudioFormat::Ogg).unwrap(), "\"ogg\"");
        assert_eq!(
            serde_json::to_string(&AudioFormat::Flac).unwrap(),
            "\"flac\""
        );
    }

    #[test]
    fn test_audio_format_deserialization() {
        assert_eq!(
            serde_json::from_str::<AudioFormat>("\"wav\"").unwrap(),
            AudioFormat::Wav
        );
        assert_eq!(
            serde_json::from_str::<AudioFormat>("\"mp3\"").unwrap(),
            AudioFormat::Mp3
        );
        assert_eq!(
            serde_json::from_str::<AudioFormat>("\"ogg\"").unwrap(),
            AudioFormat::Ogg
        );
        assert_eq!(
            serde_json::from_str::<AudioFormat>("\"flac\"").unwrap(),
            AudioFormat::Flac
        );
    }

    #[test]
    fn test_format_config_default() {
        let config = FormatConfig::default();
        assert_eq!(config.format, AudioFormat::Wav);
        assert_eq!(config.mp3_bitrate, 192);
        assert_eq!(config.ogg_bitrate, 192);
        assert_eq!(config.flac_compression, 5);
    }

    #[test]
    fn test_format_config_serialization_roundtrip() {
        let config = FormatConfig {
            format: AudioFormat::Mp3,
            mp3_bitrate: 256,
            ogg_bitrate: 192,
            flac_compression: 8,
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: FormatConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.format, config.format);
        assert_eq!(deserialized.mp3_bitrate, config.mp3_bitrate);
        assert_eq!(deserialized.ogg_bitrate, config.ogg_bitrate);
        assert_eq!(deserialized.flac_compression, config.flac_compression);
    }

    #[test]
    fn test_output_config_with_format() {
        let config = OutputConfig {
            enabled: true,
            directory: "/tmp/audio".into(),
            filename_pattern: "test_{timestamp}.mp3".into(),
            format_config: FormatConfig {
                format: AudioFormat::Mp3,
                mp3_bitrate: 320,
                ogg_bitrate: 192,
                flac_compression: 5,
            },
        };

        let json = serde_json::to_string_pretty(&config).unwrap();
        let deserialized: OutputConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.enabled, config.enabled);
        assert_eq!(deserialized.directory, config.directory);
        assert_eq!(deserialized.filename_pattern, config.filename_pattern);
        assert_eq!(deserialized.format_config.format, AudioFormat::Mp3);
        assert_eq!(deserialized.format_config.mp3_bitrate, 320);
    }

    #[test]
    fn test_output_config_missing_format_uses_default() {
        let json = r#"{
            "enabled": true,
            "directory": "/tmp",
            "filename_pattern": "test.wav"
        }"#;

        let config: OutputConfig = serde_json::from_str(json).unwrap();
        assert!(config.enabled);
        assert_eq!(config.format_config.format, AudioFormat::Wav);
        assert_eq!(config.format_config.mp3_bitrate, 192);
    }

    // ========== Validation Tests ==========

    #[test]
    fn test_validation_valid_config() {
        let config = AppConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validation_command_empty() {
        let mut config = AppConfig::default();
        config.tts.active_backend = TtsEngine::Local;
        if let Some(profile) = config
            .tts
            .profiles
            .iter_mut()
            .find(|p| p.id == config.tts.active_profile_id)
        {
            profile.engine = TtsEngine::Local;
        }
        config.tts.command = "".into();
        let result = config.validate();
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors
            .iter()
            .any(|e| matches!(e, ValidationError::CommandEmpty)));
    }

    #[test]
    fn test_validation_args_template_missing_placeholders() {
        let mut config = AppConfig::default();
        config.tts.active_backend = TtsEngine::Local;
        if let Some(profile) = config
            .tts
            .profiles
            .iter_mut()
            .find(|p| p.id == config.tts.active_profile_id)
        {
            profile.engine = TtsEngine::Local;
        }
        config.tts.args_template = vec!["-v".into(), "{voice}".into()];
        let result = config.validate();
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors
            .iter()
            .any(|e| matches!(e, ValidationError::ArgsTemplateMissingPlaceholder { .. })));
    }

    // ========== TTS Profile / Migration Tests ==========

    #[test]
    fn test_default_tts_config_uses_kokoro_profile() {
        let tts = TtsConfig::default();
        assert_eq!(TtsEngine::default(), TtsEngine::Kokoro);
        assert_eq!(tts.schema_version, 7);
        assert_eq!(tts.active_backend, TtsEngine::Kokoro);
        assert_eq!(tts.profiles.len(), 7);
        // The legacy Edge "default" profile stays (zero-download fallback).
        assert_eq!(tts.profiles[0].id, "default");
        assert_eq!(tts.profiles[0].engine, TtsEngine::Edge);
        // ...but the ACTIVE profile is the bundled Kokoro one.
        assert_eq!(tts.active_profile_id, "profile-kokoro-default");
        assert_eq!(
            tts.profiles
                .iter()
                .find(|profile| profile.id == tts.active_profile_id)
                .map(|profile| &profile.engine),
            Some(&TtsEngine::Kokoro)
        );
    }

    #[test]
    fn test_migrate_add_kokoro_profile_v6_adds_and_is_idempotent() {
        // A v5 config (no Kokoro profile) gains one; the active engine is
        // never touched by the migration.
        let mut tts = TtsConfig::default();
        tts.schema_version = 5;
        tts.profiles.retain(|p| p.id != "profile-kokoro-default");
        tts.active_backend = TtsEngine::Edge;
        tts.active_profile_id = "default".into();
        let before = tts.profiles.len();

        crate::config::tts::migrate_add_kokoro_profile_v6(&mut tts);
        assert_eq!(tts.schema_version, 6);
        assert_eq!(tts.profiles.len(), before + 1);
        assert!(tts
            .profiles
            .iter()
            .any(|p| p.id == "profile-kokoro-default" && p.engine == TtsEngine::Kokoro));
        assert_eq!(tts.active_backend, TtsEngine::Edge);
        assert_eq!(tts.active_profile_id, "default");

        // Idempotent.
        crate::config::tts::migrate_add_kokoro_profile_v6(&mut tts);
        assert_eq!(tts.profiles.len(), before + 1);
        assert_eq!(tts.schema_version, 6);
    }

    /// A legacy Piper `Local` preset exactly as `install-piper.ps1` used to
    /// write it, including the CWD-relative wrapper path that produced
    /// "voice model not found ... Available: []".
    fn legacy_piper_profile() -> VoiceProfile {
        VoiceProfile {
            id: "piper-local".into(),
            name: "Piper (Local)".into(),
            description: None,
            engine: TtsEngine::Local,
            voice: "en_US-amy-medium".into(),
            voice_label: None,
            speed: 1.0,
            pitch: 1.0,
            effects: ProfileEffects {
                enabled: false,
                active_effect: crate::config::EffectId::None,
            },
            text_processing: ProfileTextProcessing::default(),
            engine_options: ProfileEngineOptions::Local(LocalEngineOptions {
                command: Some("uv".into()),
                args_template: Some(vec![
                    "run".into(),
                    "--project".into(),
                    "{engine_dir}/piper".into(),
                    "python".into(),
                    "scripts/copyspeak-piper.py".into(),
                    "--text-file".into(),
                    "{input}".into(),
                    "--voice".into(),
                    "{voice}".into(),
                    "--output".into(),
                    "{output}".into(),
                ]),
            }),
        }
    }

    #[test]
    fn test_migrate_tts_profiles_v7_promotes_piper_and_migrates_gemini() {
        let mut tts = TtsConfig::default();
        tts.schema_version = 6;
        tts.google.model = "gemini-2.5-flash-preview-tts".into();
        tts.profiles.push(legacy_piper_profile());
        // Make the legacy Piper profile active so the schema mirror matters.
        tts.active_profile_id = "piper-local".into();
        tts.active_backend = TtsEngine::Local;

        crate::config::tts::migrate_tts_profiles_v7(&mut tts);

        assert_eq!(tts.schema_version, 7);
        let piper = tts
            .profiles
            .iter()
            .find(|p| p.id == "piper-local")
            .expect("legacy Piper profile must survive promotion");
        // Promoted to the first-class engine, so it gets the daemon + GPU toggle.
        assert_eq!(piper.engine, TtsEngine::Piper);
        assert!(matches!(
            piper.engine_options,
            ProfileEngineOptions::Piper(_)
        ));
        // The profile's own settings are kept.
        assert_eq!(piper.voice, "en_US-amy-medium");
        assert!(!piper.effects.enabled);
        // ...and the derived mirror follows the promoted engine.
        assert_eq!(tts.active_backend, TtsEngine::Piper);

        // Gemini moves off the retired preview model, globally and per profile.
        assert_eq!(tts.google.model, "gemini-3.8-flash-tts");
        let gemini = tts
            .profiles
            .iter()
            .find(|p| p.engine == TtsEngine::Google)
            .expect("bundled Gemini profile");
        match &gemini.engine_options {
            ProfileEngineOptions::Google(o) => {
                assert_eq!(o.model.as_deref(), Some("gemini-3.8-flash-tts"));
            }
            other => panic!("unexpected Gemini options: {other:?}"),
        }

        // Idempotent: a second run changes nothing.
        let once = tts.clone();
        crate::config::tts::migrate_tts_profiles_v7(&mut tts);
        assert_eq!(tts.schema_version, 7);
        assert_eq!(tts.profiles.len(), once.profiles.len());
        assert_eq!(tts.active_backend, TtsEngine::Piper);
    }

    #[test]
    fn test_migrate_tts_profiles_v7_leaves_custom_local_clis_alone() {
        // A hand-written local CLI that merely mentions the Piper wrapper (a
        // different command, or extra user flags) is not the installer's preset
        // and must not be rewritten — promoting it would replace the user's
        // command with the fixed template and discard their settings.
        let mut tts = TtsConfig::default();
        tts.schema_version = 6;
        for (id, command, args) in [
            (
                "custom-python",
                Some("C:/Python312/python.exe"),
                vec!["C:/my/copyspeak-piper.py", "--device", "cuda"],
            ),
            (
                "custom-flags",
                Some("uv"),
                vec!["run", "scripts/copyspeak-piper.py", "--my-flag", "1"],
            ),
        ] {
            let mut profile = legacy_piper_profile();
            profile.id = id.into();
            profile.engine_options = ProfileEngineOptions::Local(LocalEngineOptions {
                command: command.map(str::to_string),
                args_template: Some(args.into_iter().map(str::to_string).collect()),
            });
            tts.profiles.push(profile);
        }
        tts.profiles.push(legacy_piper_profile());

        crate::config::tts::migrate_tts_profiles_v7(&mut tts);

        for id in ["custom-python", "custom-flags"] {
            let profile = tts
                .profiles
                .iter()
                .find(|p| p.id == id)
                .unwrap_or_else(|| panic!("{id} must survive"));
            assert_eq!(
                profile.engine,
                TtsEngine::Local,
                "{id} must not be promoted to the first-class Piper engine"
            );
        }
        // The real installer preset beside them is still promoted.
        let installer = tts
            .profiles
            .iter()
            .find(|p| p.id == "piper-local")
            .expect("installer Piper profile");
        assert_eq!(installer.engine, TtsEngine::Piper);
    }

    #[test]
    fn test_bundled_kokoro_profile_disables_effects_for_streaming() {
        // The bundled local engines ship with effects off (as their installers
        // write), because an enabled effect forces the fragment path and makes
        // the whole fragment synthesize before any audio plays.
        let tts = TtsConfig::default();
        let kokoro = tts
            .profiles
            .iter()
            .find(|p| p.id == "profile-kokoro-default")
            .expect("bundled Kokoro profile");
        assert!(
            !kokoro.effects.enabled,
            "bundled Kokoro must not enable an effect, or it loses streaming playback"
        );
        assert_eq!(kokoro.effects.active_effect, crate::config::EffectId::None);
    }

    #[test]
    fn test_migrate_legacy_config_creates_default_profile() {
        // Simulate a v0 config: no profiles, ElevenLabs active.
        let mut tts = TtsConfig::default();
        tts.schema_version = 0;
        tts.profiles = Vec::new();
        tts.active_backend = TtsEngine::ElevenLabs;
        tts.elevenlabs.voice_id = "voice-xyz".into();

        let migrated = migrate_tts_config(tts);
        assert_eq!(migrated.schema_version, 2);
        assert_eq!(migrated.active_profile_id, "default");
        assert_eq!(migrated.profiles.len(), 1);
        assert_eq!(migrated.profiles[0].engine, TtsEngine::ElevenLabs);
        assert_eq!(migrated.profiles[0].voice, "voice-xyz");
    }

    #[test]
    fn test_migrate_is_idempotent() {
        let tts = TtsConfig::default();
        let once = migrate_tts_config(tts);
        let twice = migrate_tts_config(once.clone());
        assert_eq!(once.profiles.len(), twice.profiles.len());
        assert_eq!(twice.schema_version, 2);
    }

    #[test]
    fn test_profile_json_roundtrip() {
        let profile = VoiceProfile {
            id: "p1".into(),
            name: "P1".into(),
            description: None,
            engine: TtsEngine::Http,
            voice: "amy.wav".into(),
            voice_label: None,
            speed: 1.05,
            pitch: 1.0,
            effects: ProfileEffects {
                enabled: true,
                active_effect: EffectId::WalkieTalkie,
            },
            text_processing: crate::config::ProfileTextProcessing::default(),
            engine_options: ProfileEngineOptions::Http(HttpEngineOptions {
                body_template: Some(r#"{"model":"tts"}"#.into()),
                ..HttpEngineOptions::default()
            }),
        };
        let json = serde_json::to_string(&profile).unwrap();
        let back: VoiceProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "p1");
        assert_eq!(back.engine, TtsEngine::Http);
        assert!(back.effects.enabled);
        assert_eq!(back.effects.active_effect, EffectId::WalkieTalkie);
    }

    #[test]
    fn test_validation_double_copy_window_too_small() {
        let mut config = AppConfig::default();
        config.trigger.double_copy_window_ms = 50;
        let result = config.validate();
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors
            .iter()
            .any(|e| matches!(e, ValidationError::DoubleCopyWindowTooSmall { .. })));
    }

    #[test]
    fn default_profile_speed_and_pitch_are_neutral() {
        let profile = VoiceProfile::default();
        assert_eq!(profile.speed, 1.0);
        assert_eq!(profile.pitch, 1.0);
    }

    #[test]
    fn loaded_config_gets_current_version_stamp() {
        let mut config = AppConfig::default();
        config.version = "0.1.10-fossil".into();
        let stamped = stamp_config_version(config);
        assert_eq!(stamped.version, CONFIG_VERSION);
    }
}
