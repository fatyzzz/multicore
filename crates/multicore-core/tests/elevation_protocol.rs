use multicore_core::elevation_protocol::{
    BrokerErrorCode, ElevatedEngine, ElevationCommand, ElevationResponse,
    MAX_ELEVATION_FRAME_BYTES, ProtocolError, SessionSecret, decode_command, decode_frame,
    encode_frame, response_matches_command,
};

fn raw(payload: &[u8]) -> Vec<u8> {
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(payload);
    frame
}

#[test]
fn accepts_only_fixed_commands_and_valid_generation_ids() {
    for command in [
        ElevationCommand::StartXray { generation_id: 7 },
        ElevationCommand::StartMihomo { generation_id: 8 },
        ElevationCommand::Diagnostics,
        ElevationCommand::Shutdown,
    ] {
        assert_eq!(
            decode_command(&encode_frame(&command).unwrap()),
            Ok(command)
        );
    }
    assert_eq!(
        decode_command(&raw(
            br#"{"version":1,"body":{"start_xray":{"generation_id":0}}}"#
        )),
        Err(ProtocolError::InvalidGeneration)
    );
}

#[test]
fn rejects_length_truncation_trailing_and_utf8_failures() {
    assert_eq!(
        decode_command(&0_u32.to_le_bytes()),
        Err(ProtocolError::InvalidLength)
    );
    assert_eq!(
        decode_command(&((MAX_ELEVATION_FRAME_BYTES - 3) as u32).to_le_bytes()),
        Err(ProtocolError::InvalidLength)
    );
    assert_eq!(
        decode_command(&[5, 0, 0, 0, b'{']),
        Err(ProtocolError::Truncated)
    );
    let mut valid = encode_frame(&ElevationCommand::Shutdown).unwrap();
    valid.push(0);
    assert_eq!(decode_command(&valid), Err(ProtocolError::TrailingBytes));
    assert_eq!(
        decode_command(&raw(&[0xff])),
        Err(ProtocolError::InvalidUtf8)
    );

    let exact_maximum = raw(&vec![b' '; MAX_ELEVATION_FRAME_BYTES - 4]);
    assert_eq!(exact_maximum.len(), MAX_ELEVATION_FRAME_BYTES);
    assert_eq!(
        decode_command(&exact_maximum),
        Err(ProtocolError::InvalidSchema)
    );
    assert_eq!(
        encode_frame(&"x".repeat(MAX_ELEVATION_FRAME_BYTES)),
        Err(ProtocolError::InvalidLength)
    );
}

#[test]
fn responses_also_use_an_exact_bounded_schema() {
    let response = ElevationResponse::Diagnostics {
        xray_running: false,
        mihomo_running: true,
    };
    assert_eq!(
        decode_frame(&encode_frame(&response).unwrap()),
        Ok(response)
    );
    let extra = raw(
        br#"{"version":1,"body":{"diagnostics":{"xray_running":false,"mihomo_running":false,"secret":"no"}}}"#,
    );
    assert_eq!(
        decode_frame::<ElevationResponse>(&extra),
        Err(ProtocolError::InvalidSchema)
    );
}

#[test]
fn every_response_is_paired_with_its_originating_command() {
    let cases = [
        (
            ElevationCommand::StartXray { generation_id: 1 },
            ElevationResponse::Started {
                engine: ElevatedEngine::Xray,
            },
            ElevationResponse::Started {
                engine: ElevatedEngine::Mihomo,
            },
        ),
        (
            ElevationCommand::StartMihomo { generation_id: 1 },
            ElevationResponse::Started {
                engine: ElevatedEngine::Mihomo,
            },
            ElevationResponse::Started {
                engine: ElevatedEngine::Xray,
            },
        ),
        (
            ElevationCommand::Stop {
                engine: ElevatedEngine::Xray,
            },
            ElevationResponse::Stopped {
                engine: ElevatedEngine::Xray,
            },
            ElevationResponse::Stopped {
                engine: ElevatedEngine::Mihomo,
            },
        ),
        (
            ElevationCommand::Diagnostics,
            ElevationResponse::Diagnostics {
                xray_running: false,
                mihomo_running: false,
            },
            ElevationResponse::ShuttingDown,
        ),
        (
            ElevationCommand::Shutdown,
            ElevationResponse::ShuttingDown,
            ElevationResponse::Diagnostics {
                xray_running: false,
                mihomo_running: false,
            },
        ),
    ];
    for (command, valid, mismatch) in cases {
        assert!(response_matches_command(&command, &valid));
        assert!(!response_matches_command(&command, &mismatch));
        assert!(response_matches_command(
            &command,
            &ElevationResponse::Error {
                code: BrokerErrorCode::NotReady
            }
        ));
    }
}

#[test]
fn rejects_unknown_version_command_fields_and_duplicate_fields() {
    for payload in [
        br#"{"version":2,"body":"shutdown"}"#.as_slice(),
        br#"{"version":1,"body":"execute"}"#,
        br#"{"version":1,"body":{"shutdown":{"path":"C:\\evil.exe"}}}"#,
        br#"{"version":1,"version":1,"body":"shutdown"}"#,
        br#"{"version":1,"body":{"start_xray":{"generation_id":1,"generation_id":2}}}"#,
    ] {
        assert!(
            decode_command(&raw(payload)).is_err(),
            "accepted {}",
            String::from_utf8_lossy(payload)
        );
    }
}

#[test]
fn broker_secret_is_redacted_and_authentication_is_exact() {
    let secret = SessionSecret::from_bytes([0x41; 32]);
    let proof = secret.authentication();
    assert!(secret.verifies(&proof));
    assert!(!SessionSecret::from_bytes([0x42; 32]).verifies(&proof));
    assert!(!format!("{secret:?}").contains("41"));
    assert!(!format!("{proof:?}").contains("41"));
    let decoded = decode_frame(&encode_frame(&proof).unwrap()).unwrap();
    assert!(secret.verifies(&decoded));
}
