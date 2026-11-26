//! Length-prefixed JSON frames: blocking for the worker, async for the supervisor.
//!
//! A frame is a `u32` little-endian body length and a JSON body. EOF before
//! a length is a clean end (`Ok(None)`); EOF inside a frame is `Truncated`;
//! a length above [`MAX_FRAME_BYTES`] is `TooLarge`, found before anything
//! is allocated.

use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest frame body, 16 MiB.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

const HEADER: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("failed to move a frame: {0}")]
    Io(#[from] io::Error),
    #[error("failed to read a frame: the stream ended inside it")]
    Truncated,
    #[error("failed to accept a frame of {len} bytes: the limit is {MAX_FRAME_BYTES}")]
    TooLarge { len: u64 },
    #[error("failed to decode a frame: {0}")]
    Json(#[from] serde_json::Error),
}

/// Encodes `msg` as one frame into `buf`, replacing its contents.
pub fn encode<T: Serialize>(msg: &T, buf: &mut Vec<u8>) -> Result<(), FrameError> {
    buf.clear();
    buf.extend_from_slice(&[0; HEADER]);
    serde_json::to_writer(&mut *buf, msg)?;
    let len = buf.len() - HEADER;
    let len32 = u32::try_from(len)
        .ok()
        .filter(|&n| n <= MAX_FRAME_BYTES)
        .ok_or(FrameError::TooLarge { len: len as u64 })?;
    buf[..HEADER].copy_from_slice(&len32.to_le_bytes());
    Ok(())
}

/// Decodes a frame body.
pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    Ok(serde_json::from_slice(body)?)
}

fn check_len(header: [u8; HEADER]) -> Result<usize, FrameError> {
    let len = u32::from_le_bytes(header);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge { len: len.into() });
    }
    Ok(len as usize)
}

fn truncated(err: io::Error) -> FrameError {
    if err.kind() == io::ErrorKind::UnexpectedEof {
        FrameError::Truncated
    } else {
        FrameError::Io(err)
    }
}

/// Writes one frame with a single `write_all`. `buf` is scratch space.
pub fn write_frame<W: Write, T: Serialize>(
    w: &mut W,
    msg: &T,
    buf: &mut Vec<u8>,
) -> Result<(), FrameError> {
    encode(msg, buf)?;
    w.write_all(buf)?;
    w.flush()?;
    Ok(())
}

/// Reads one frame. `buf` is scratch space for the body.
pub fn read_frame<R: Read, T: DeserializeOwned>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Option<T>, FrameError> {
    let mut header = [0; HEADER];
    let mut got = 0;
    while got < HEADER {
        match r.read(&mut header[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(n) => got += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(FrameError::Io(err)),
        }
    }
    let len = check_len(header)?;
    buf.clear();
    buf.resize(len, 0);
    r.read_exact(buf).map_err(truncated)?;
    decode(buf).map(Some)
}

/// Async [`write_frame`]. Not cancel safe: a dropped write leaves a partial
/// frame, so the caller kills the worker.
pub async fn write_frame_async<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    msg: &T,
    buf: &mut Vec<u8>,
) -> Result<(), FrameError> {
    encode(msg, buf)?;
    w.write_all(buf).await?;
    w.flush().await?;
    Ok(())
}

/// Async [`read_frame`]. Not cancel safe, as for the writer.
pub async fn read_frame_async<R: AsyncRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Option<T>, FrameError> {
    let mut header = [0; HEADER];
    let mut got = 0;
    while got < HEADER {
        match r.read(&mut header[got..]).await {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(n) => got += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(FrameError::Io(err)),
        }
    }
    let len = check_len(header)?;
    buf.clear();
    buf.resize(len, 0);
    r.read_exact(buf).await.map_err(truncated)?;
    decode(buf).map(Some)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use super::*;
    use crate::ipc::protocol::*;

    fn round_trip<T>(msg: T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let mut buf = Vec::new();
        let mut wire = Vec::new();
        write_frame(&mut wire, &msg, &mut buf).unwrap();
        let mut r = Cursor::new(wire);
        let back: T = read_frame(&mut r, &mut buf).unwrap().unwrap();
        assert_eq!(back, msg);
        assert!(read_frame::<_, T>(&mut r, &mut buf).unwrap().is_none());
    }

    fn call(op: Op) -> Request {
        Request::Call(Call { id: 7, op })
    }

    fn requests() -> Vec<Request> {
        let pw = Some(Secret::new("pw"));
        vec![
            Request::Init(Init {
                protocol: PROTOCOL_VERSION,
                lo_program_dir: Some(PathBuf::from("/opt/libreoffice/program")),
                pdfium_lib_dir: None,
                warmup: true,
                test: TestInit {
                    fatal: true,
                    ready_delay_ms: 5,
                },
            }),
            call(Op::Ping),
            call(Op::Convert(ConvertArgs {
                input: "/j/source.docx".into(),
                output: "/j/document.pdf".into(),
                family: Family::Presentation,
                password: pw.clone(),
                export: ExportOptions {
                    notes: SlideNotes::Only,
                    include_hidden_slides: true,
                    one_page_per_sheet: false,
                },
                max_output_bytes: 1 << 30,
            })),
            call(Op::Inspect(InspectArgs {
                pdf: "/j/document.pdf".into(),
                password: None,
                probe_dpi: Some(18),
                probe_pages: ProbePages::Only(vec![1, 3]),
                collect_fonts: true,
            })),
            call(Op::Render(RenderArgs {
                pdf: "/j/document.pdf".into(),
                password: pw.clone(),
                pages: vec![1, 2, 3],
                format: ImageFormat::Png,
                dpi: 150,
                quality: 85,
                max_edge: 4000,
                out_dir: "/j/pages".into(),
                max_output_bytes: 1000,
            })),
            call(Op::Decrypt(DecryptArgs {
                pdf: "/j/source.pdf".into(),
                password: Secret::new("x"),
                output: "/j/document.pdf".into(),
                max_output_bytes: 2,
                check_owner: true,
            })),
            call(Op::Test(TestOp::Sleep { ms: 3 })),
            call(Op::Test(TestOp::Hang)),
            call(Op::Test(TestOp::Abort)),
            call(Op::Test(TestOp::Exit { code: 3 })),
            call(Op::Test(TestOp::Allocate { mb: 64 })),
            call(Op::Test(TestOp::WriteStdout { bytes: 4096 })),
            call(Op::Test(TestOp::SpawnSleeper)),
            call(Op::Test(TestOp::Env)),
        ]
    }

    fn reply(result: Result<Output, WorkerError>) -> Response {
        Response::Reply(Reply {
            id: 7,
            job: 2,
            engine: EngineTimes {
                engine_us: 10,
                render_us: 4,
                encode_us: 6,
                ..EngineTimes::default()
            },
            rss_bytes: 1 << 20,
            result,
        })
    }

    fn responses() -> Vec<Response> {
        let page = RenderedPage {
            number: 1,
            file_name: "0001.jpg".into(),
            width_px: 1240,
            height_px: 1754,
            width_pt: 595.0,
            height_pt: 842.0,
            size_bytes: 4567,
            objects: 3,
            flat: false,
            colour: [255, 255, 255],
            effective_dpi: 150.0,
            crc32: Some(7),
        };
        let mut out = vec![
            Response::Ready(Ready {
                protocol: PROTOCOL_VERSION,
                pid: 42,
                engine_version: "26.8.3.1".into(),
                bind_us: 61_000,
                rss_bytes: 96 << 20,
            }),
            Response::Fatal(Fatal {
                message: "failed to bind the engine".into(),
            }),
            reply(Ok(Output::Pong)),
            reply(Ok(Output::Converted(ConvertedOut {
                pdf_bytes: 1234,
                doc_type: "text".into(),
                facts: Some(Box::new(SourceFactsWire {
                    declared_pages: Some(DeclaredPagesWire {
                        count: 3,
                        source: "ppt/presentation.xml".into(),
                    }),
                    requested_fonts: vec!["Calibri".into()],
                    embedded_fonts: vec!["Aptos".into()],
                    macros: Some(MacroFactWire {
                        kind: MacroKindWire::Vba,
                        part: "word/vbaProject.bin".into(),
                    }),
                    skipped: vec![(CheckWire::Fonts, SkipReasonWire::CapReached)],
                })),
            }))),
            reply(Ok(Output::Inspected(InspectedOut {
                page_count: 1,
                encrypted: true,
                needs_password: false,
                probe_skipped: 2,
                pages: vec![PageGeometry {
                    width_pt: 595.0,
                    height_pt: 842.0,
                }],
                probe: vec![ProbedPage {
                    number: 1,
                    objects: 0,
                    flat: true,
                    colour: [1, 2, 3],
                }],
                fonts: vec!["Carlito".into()],
            }))),
            reply(Ok(Output::Rendered(RenderedOut { pages: vec![page] }))),
            reply(Ok(Output::Decrypted(DecryptedOut { pdf_bytes: 98 }))),
            reply(Ok(Output::Test(TestOutput::Slept))),
            reply(Ok(Output::Test(TestOutput::Allocated { rss_bytes: 5 }))),
            reply(Ok(Output::Test(TestOutput::Wrote { bytes: 4096 }))),
            reply(Ok(Output::Test(TestOutput::Spawned { pid: 9 }))),
            reply(Ok(Output::Test(TestOutput::Env {
                vars: vec![("HOME".into(), "/slot".into())],
            }))),
        ];
        for kind in [
            WorkerErrorKind::PasswordRequired,
            WorkerErrorKind::InvalidPassword,
            WorkerErrorKind::LoadFailed,
            WorkerErrorKind::ExportFailed,
            WorkerErrorKind::EmptyDocument,
            WorkerErrorKind::LimitExceeded,
            WorkerErrorKind::DiskFull,
            WorkerErrorKind::Io,
            WorkerErrorKind::Unsupported,
            WorkerErrorKind::Internal,
            WorkerErrorKind::FakeQueueFull,
            WorkerErrorKind::FakeUnavailable,
        ] {
            out.push(reply(Err(WorkerError::new(
                kind,
                "failed to load document",
            ))));
        }
        out
    }

    #[test]
    fn every_request_round_trips() {
        for msg in requests() {
            round_trip(msg);
        }
    }

    #[test]
    fn every_response_round_trips() {
        for msg in responses() {
            round_trip(msg);
        }
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = Secret::new("hunter2");
        assert_eq!(format!("{secret:?}"), "***");
        let args = DecryptArgs {
            pdf: "/a".into(),
            password: secret,
            output: "/b".into(),
            max_output_bytes: 1,
            check_owner: false,
        };
        assert!(!format!("{args:?}").contains("hunter2"));
    }

    #[test]
    fn eof_before_a_length_is_none() {
        let mut buf = Vec::new();
        let got: Option<Request> = read_frame(&mut Cursor::new(Vec::new()), &mut buf).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn eof_inside_the_length_is_truncated() {
        let mut buf = Vec::new();
        let err = read_frame::<_, Request>(&mut Cursor::new(vec![5, 0]), &mut buf).unwrap_err();
        assert!(matches!(err, FrameError::Truncated), "{err:?}");
    }

    #[test]
    fn truncated_body_is_an_error() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &call(Op::Ping), &mut Vec::new()).unwrap();
        wire.truncate(wire.len() - 3);
        let err = read_frame::<_, Request>(&mut Cursor::new(wire), &mut Vec::new()).unwrap_err();
        assert!(matches!(err, FrameError::Truncated), "{err:?}");
    }

    #[test]
    fn oversize_length_is_refused_before_allocating() {
        let mut wire = (MAX_FRAME_BYTES + 1).to_le_bytes().to_vec();
        wire.extend_from_slice(b"{}");
        let mut buf = Vec::new();
        let err = read_frame::<_, Request>(&mut Cursor::new(wire), &mut buf).unwrap_err();
        assert!(
            matches!(err, FrameError::TooLarge { len } if len == u64::from(MAX_FRAME_BYTES) + 1)
        );
        assert_eq!(buf.capacity(), 0, "nothing may be allocated for the body");
    }

    #[test]
    fn oversize_message_is_not_encoded() {
        let big = Fatal {
            message: "x".repeat(MAX_FRAME_BYTES as usize),
        };
        let err = encode(&Response::Fatal(big), &mut Vec::new()).unwrap_err();
        assert!(matches!(err, FrameError::TooLarge { .. }));
    }

    #[test]
    fn bad_json_is_an_error() {
        let mut wire = 2u32.to_le_bytes().to_vec();
        wire.extend_from_slice(b"{]");
        let err = read_frame::<_, Request>(&mut Cursor::new(wire), &mut Vec::new()).unwrap_err();
        assert!(matches!(err, FrameError::Json(_)));
    }

    #[tokio::test]
    async fn async_writer_feeds_sync_reader_over_a_socket_pair() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let mut a = tokio::net::UnixStream::from_std(a).unwrap();
        let sent = requests();
        let expected = sent.clone();
        let reader = std::thread::spawn(move || {
            let mut b = b;
            let mut buf = Vec::new();
            let mut got = Vec::new();
            while let Some(msg) = read_frame::<_, Request>(&mut b, &mut buf).unwrap() {
                got.push(msg);
            }
            got
        });
        let mut buf = Vec::new();
        for msg in &sent {
            write_frame_async(&mut a, msg, &mut buf).await.unwrap();
        }
        drop(a);
        assert_eq!(reader.join().unwrap(), expected);
    }

    #[tokio::test]
    async fn sync_writer_feeds_async_reader() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        b.set_nonblocking(true).unwrap();
        let mut b = tokio::net::UnixStream::from_std(b).unwrap();
        let sent = responses();
        let to_send = sent.clone();
        let writer = std::thread::spawn(move || {
            let mut a = a;
            let mut buf = Vec::new();
            for msg in &to_send {
                write_frame(&mut a, msg, &mut buf).unwrap();
            }
        });
        let mut buf = Vec::new();
        let mut got = Vec::new();
        while let Some(msg) = read_frame_async::<_, Response>(&mut b, &mut buf)
            .await
            .unwrap()
        {
            got.push(msg);
        }
        writer.join().unwrap();
        assert_eq!(got, sent);
    }
}
