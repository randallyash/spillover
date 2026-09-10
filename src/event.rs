//! Keyboard and resize events, forwarded from a dedicated input thread.

use crossterm::event::{self, Event, KeyEvent};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub enum InputEvent {
    Key(KeyEvent),
    Resize { cols: u16, rows: u16 },
}

/// Read terminal events on a blocking thread and forward them to the UI loop.
///
/// A single long-lived thread is used deliberately: spawning a blocking task per
/// iteration would leave orphans racing each other on `event::read()`, which
/// loses keystrokes.
pub fn spawn_input_thread(tx: UnboundedSender<InputEvent>) {
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            let forwarded = match event {
                Event::Key(key) => tx.send(InputEvent::Key(key)),
                Event::Resize(cols, rows) => tx.send(InputEvent::Resize { cols, rows }),
                _ => continue,
            };
            if forwarded.is_err() {
                break;
            }
        }
    });
}
