//! Keyboard and resize events, forwarded from a dedicated input thread.

use crossterm::event::{self, Event, KeyEvent};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub enum InputEvent {
    Key(KeyEvent),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Text pasted as one block, which bracketed paste makes possible.
    Paste(String),
}

/// Read terminal events on a blocking thread and forward them to the UI loop.
pub fn spawn_input_thread(tx: UnboundedSender<InputEvent>) {
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            let forwarded = match event {
                Event::Key(key) => tx.send(InputEvent::Key(key)),
                Event::Resize(cols, rows) => tx.send(InputEvent::Resize { cols, rows }),
                // Only delivered because the terminal guard enables bracketed
                // paste; without it a paste arrives as individual key events.
                Event::Paste(text) => tx.send(InputEvent::Paste(text)),
                _ => continue,
            };
            if forwarded.is_err() {
                break;
            }
        }
    });
}
