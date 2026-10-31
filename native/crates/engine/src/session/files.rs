//! File transfer glue: the viewer's push loop and the host's "behave like a real
//! drop" delivery of a received file.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use platform_api::{FileDrop, InputInjector, Platform};
use protocol::InputMsg;

/// Read each dropped file and emit it as bulk frames. One file at a time: the
/// bulk channel is ordered and a transfer has no id, so overlapping them would
/// interleave two files into one.
pub(super) fn file_send_loop(
    drops: Receiver<FileDrop>,
    out: Sender<Vec<u8>>,
    ui: tokio::sync::mpsc::UnboundedSender<crate::UiEvent>,
    stop: Arc<AtomicBool>,
) {
    use protocol::transfer::{BulkFrame, FileMeta, CHUNK_BYTES, MAX_FILE_BYTES};
    use std::io::Read;

    while !stop.load(Ordering::SeqCst) {
        let Ok(FileDrop {
            paths,
            nx: drop_x,
            ny: drop_y,
        }) = drops.recv_timeout(std::time::Duration::from_millis(200))
        else {
            continue;
        };
        for path in paths {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let Ok(meta) = std::fs::metadata(&path) else {
                let _ = ui.send(crate::UiEvent::Toast(format!("Cannot read {name}")));
                continue;
            };
            if meta.is_dir() {
                // Folders would need a manifest and per-entry paths — exactly the
                // shape that invites traversal bugs. Files only, for now.
                let _ = ui.send(crate::UiEvent::Toast(format!(
                    "{name} is a folder — send files instead"
                )));
                continue;
            }
            let size = meta.len();
            if size > MAX_FILE_BYTES {
                let _ = ui.send(crate::UiEvent::Toast(format!("{name} is too large")));
                continue;
            }
            let Ok(mut file) = std::fs::File::open(&path) else {
                let _ = ui.send(crate::UiEvent::Toast(format!("Cannot open {name}")));
                continue;
            };

            let _ = ui.send(crate::UiEvent::Toast(format!("Sending {name}…")));
            let _ = out.send(
                BulkFrame::Begin(FileMeta {
                    name: name.clone(),
                    size,
                    // Where it was dropped, so the host can paste it into the
                    // window under that point rather than just filing it away.
                    drop_at: Some((drop_x, drop_y)),
                })
                .encode(),
            );

            let mut buf = vec![0u8; CHUNK_BYTES];
            let mut sent: u64 = 0;
            let mut failed = false;
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if out
                            .send(BulkFrame::Chunk(buf[..n].to_vec()).encode())
                            .is_err()
                        {
                            failed = true; // session ended under us
                            break;
                        }
                        sent += n as u64;
                    }
                    Err(e) => {
                        let _ = out.send(BulkFrame::Abort(format!("read error: {e}")).encode());
                        let _ = ui.send(crate::UiEvent::Toast(format!("Failed sending {name}")));
                        failed = true;
                        break;
                    }
                }
                if stop.load(Ordering::SeqCst) {
                    return;
                }
            }
            if failed {
                continue;
            }
            if sent != size {
                // The file changed while we read it — tell the peer rather than
                // let it write a file that doesn't match what was announced.
                let _ = out.send(BulkFrame::Abort("file changed while sending".into()).encode());
                let _ = ui.send(crate::UiEvent::Toast(format!(
                    "{name} changed while sending"
                )));
                continue;
            }
            let _ = out.send(BulkFrame::End.encode());
        }
    }
}

/// Make a just-received file behave like one dropped locally.
///
/// Windows gives no way to synthesize a real OLE drop into another process, so
/// this does the next best thing, which every app that accepts a pasted file
/// treats identically: put the file on the clipboard as a `CF_HDROP` file-drop
/// list (exactly what Explorer writes when you copy a file), focus the window
/// the sender aimed at, and press Ctrl+V.
///
/// Focus is set WITHOUT clicking: a real drop doesn't click either, and a
/// synthetic click could press whatever button sits under the drop point.
/// Returns whether the paste was actually attempted; the file is on the
/// clipboard (and on disk) either way.
/// Make a just-received file behave like one dropped locally.
///
/// No OS lets one process synthesize a real drag-and-drop into another, so this
/// does the next best thing, which every app that accepts a pasted file treats
/// identically: put the file on the clipboard as a real file reference (what the
/// file manager writes when you copy a file), focus the window the sender aimed
/// at, and press the platform's paste chord.
///
/// Focus is set WITHOUT clicking: a real drop doesn't click either, and a
/// synthetic click could press whatever button sits under the drop point.
/// Returns whether the paste was actually attempted; the file is on the
/// clipboard (and on disk) either way.
pub(super) fn deliver_received_file(
    path: &std::path::Path,
    drop_at: Option<(f64, f64)>,
    injector: Option<&mut dyn InputInjector>,
    platform: &dyn Platform,
    paste_dropped: bool,
) -> bool {
    let clipboard = platform.create_clipboard();
    if clipboard
        .set_files(std::slice::from_ref(&path.to_path_buf()))
        .is_none()
    {
        tracing::warn!("could not put {path:?} on the clipboard");
        return false;
    }
    if !paste_dropped {
        return false;
    }
    let (Some((nx, ny)), Some(inj)) = (drop_at, injector) else {
        return false; // saved + on the clipboard; the user pastes it themselves
    };
    if !inj.focus_window_at(nx, ny) {
        tracing::info!("no window under the drop point — file left on the clipboard");
        return false;
    }
    // Give the focused window a moment to become foreground before typing.
    std::thread::sleep(std::time::Duration::from_millis(120));
    let chord = inj.paste_chord();
    press_chord(inj, chord);
    tracing::info!("pasted {path:?} at ({nx:.3}, {ny:.3})");
    true
}

/// Press `keys` in order, release them in reverse (a modifier chord).
fn press_chord(inj: &mut dyn InputInjector, keys: &[&str]) {
    for code in keys {
        inj.dispatch(&InputMsg::KeyDown {
            code: (*code).to_string(),
        });
    }
    for code in keys.iter().rev() {
        inj.dispatch(&InputMsg::KeyUp {
            code: (*code).to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::mock::MockPlatform;

    fn temp_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sct-files-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, b"x").unwrap();
        p
    }

    #[test]
    fn delivery_pastes_with_the_platform_chord_without_clicking() {
        let p = MockPlatform::new(&std::env::temp_dir());
        let file = temp_file("a.txt");
        let mut inj = p.create_injector().unwrap();
        assert!(deliver_received_file(
            &file,
            Some((0.2, 0.3)),
            Some(inj.as_mut()),
            &p,
            true
        ));
        let log = p.injector.lock().unwrap();
        assert_eq!(log.focus_requests, vec![(0.2, 0.3)]);
        let codes: Vec<String> = log
            .events
            .iter()
            .map(|e| match e {
                InputMsg::KeyDown { code } => format!("+{code}"),
                InputMsg::KeyUp { code } => format!("-{code}"),
                other => panic!("unexpected {other:?} (a click would be a bug)"),
            })
            .collect();
        assert_eq!(codes, ["+ControlLeft", "+KeyV", "-KeyV", "-ControlLeft"]);
        assert_eq!(p.clipboard.0.lock().unwrap().files, vec![file]);
    }

    #[test]
    fn delivery_only_files_it_when_pasting_is_off_or_no_window() {
        let mut p = MockPlatform::new(&std::env::temp_dir());
        let file = temp_file("b.txt");
        let mut inj = p.create_injector().unwrap();
        assert!(!deliver_received_file(
            &file,
            Some((0.5, 0.5)),
            Some(inj.as_mut()),
            &p,
            false
        ));
        assert!(p.injector.lock().unwrap().events.is_empty());

        p.focus_result = false;
        let mut inj = p.create_injector().unwrap();
        assert!(!deliver_received_file(
            &file,
            Some((0.5, 0.5)),
            Some(inj.as_mut()),
            &p,
            true
        ));
        assert!(p.injector.lock().unwrap().events.is_empty());
        assert_eq!(p.clipboard.0.lock().unwrap().files, vec![file]);
    }
}
