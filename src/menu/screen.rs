use crate::language::tr;
use crate::AppResult;
use crossterm::{
    cursor,
    event::KeyCode,
    execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, BufWriter, Write};
use unicode_width::UnicodeWidthChar;

type Row = (Color, String);
pub(super) struct Screen {
    previous: Vec<Row>,
    offset: usize,
    limit: usize,
    size: (u16, u16),
}

impl Screen {
    pub fn enter() -> AppResult<Self> {
        terminal::enable_raw_mode()?;
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen, cursor::Hide) {
            let _ = execute!(io::stdout(), cursor::Show, LeaveAlternateScreen);
            let _ = terminal::disable_raw_mode();
            return Err(e.into());
        }
        Ok(Self {
            previous: vec![],
            offset: 0,
            limit: 0,
            size: (0, 0),
        })
    }
    pub fn top(&mut self) {
        self.offset = 0;
    }
    pub fn resize(&mut self) {
        self.size = (0, 0);
    }
    pub fn scroll(&mut self, key: KeyCode) -> bool {
        let step = usize::from(self.size.1.saturating_sub(3)).max(1);
        match key {
            KeyCode::PageUp => self.offset = self.offset.saturating_sub(step),
            KeyCode::PageDown => self.offset = (self.offset + step).min(self.limit),
            _ => return false,
        }
        true
    }
    pub fn draw(&mut self, title: &str, lines: &[String]) -> AppResult<()> {
        let size = terminal::size()?;
        let width = usize::from(size.0.saturating_sub(1)).clamp(1, 110);
        let height = usize::from(size.1);
        let mut body = Vec::new();
        for line in lines {
            let color = if line.starts_with('[') {
                Color::Cyan
            } else if line.starts_with('●') {
                Color::Green
            } else {
                Color::Reset
            };
            for text in wrap(line, width) {
                body.push((color, text));
            }
        }
        let room = height.saturating_sub(1);
        let overflow = body.len() > room;
        let visible = room.saturating_sub(usize::from(overflow));
        self.limit = body.len().saturating_sub(visible);
        self.offset = self.offset.min(self.limit);
        let mut rows = vec![(
            Color::Cyan,
            wrap(&format!("RADIO RECORDER  /  {title}"), width).remove(0),
        )];
        rows.extend(body.into_iter().skip(self.offset).take(visible));
        if overflow {
            rows.push((
                Color::DarkGrey,
                wrap(tr("PgUp / PgDn · scroll", "PgUp / PgDn · прокрутка"), width).remove(0),
            ));
        }
        rows.truncate(height);
        let mut out = BufWriter::new(io::stdout().lock());
        let resized = size != self.size;
        for i in 0..rows.len().max(self.previous.len()).min(height) {
            if !resized && rows.get(i) == self.previous.get(i) {
                continue;
            }
            queue!(out, cursor::MoveTo(0, i as u16))?;
            if let Some((color, text)) = rows.get(i) {
                queue!(out, SetForegroundColor(*color), Print(text), ResetColor)?;
            }
            queue!(out, Clear(ClearType::UntilNewLine))?;
        }
        out.flush()?;
        self.size = size;
        self.previous = rows;
        Ok(())
    }
}

fn wrap(line: &str, width: usize) -> Vec<String> {
    let mut rows = vec![];
    let mut row = String::new();
    let mut used = 0;
    for ch in line.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        let w = ch.width().unwrap_or(0);
        if used + w > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        if w <= width {
            row.push(ch);
            used += w;
        }
    }
    rows.push(row);
    rows
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), ResetColor, cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;
    #[test]
    fn wrapping_fits_console_cells_in_both_languages() {
        for width in [2, 5, 12, 74] {
            for label in ["[3]  Настройки", "[3]  Settings", "D:/Музыка/日本語/🎧.wav"]
            {
                let rows = wrap(label, width);
                assert_eq!(rows.concat(), label);
                assert!(rows.iter().all(|s| s.as_str().width() <= width));
            }
        }
    }
}
