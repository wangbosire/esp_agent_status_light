use std::io::{IsTerminal, Read};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::model::{AppError, AppResult};

pub(super) fn read_stdin_json() -> AppResult<Option<Value>> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(None);
    }

    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut raw = String::new();
        let result = stdin.lock().read_to_string(&mut raw).map(|_| raw);
        let _ = tx.send(result);
    });

    let raw = match rx.recv_timeout(Duration::from_millis(75)) {
        Ok(Ok(raw)) => raw,
        Ok(Err(err)) => return Err(AppError::io("read hook stdin", err)),
        Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
        Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
    };

    if raw.trim().is_empty() {
        return Ok(None);
    }

    match serde_json::from_str(raw.trim()) {
        Ok(value) => Ok(Some(value)),
        Err(_) => Ok(Some(json!({}))),
    }
}
