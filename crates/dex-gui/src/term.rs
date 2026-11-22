//! The viewer's Terminal tab, on Unix: the user's `$SHELL` running in a real pseudo-terminal,
//! so everything a terminal does works — readline prompts and history, `Ctrl-C`, `Ctrl-D`,
//! REPLs and programs that read stdin. Keys are forwarded to the PTY as bytes
//! ([`key_bytes`]); the shell's output is rendered by [`TermScreen`], a minimal line renderer:
//! it honours the sequences a prompt and a scroll of command output use (carriage return,
//! backspace, tab, erase-in-line, clear-screen) and strips the rest (colours, cursor
//! addressing), so full-screen programs (`vim`, `htop`) are still out of scope.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;

use rune::{Key, KeyEvent};

/// Terminal size reported to the shell. The pane is 460-640pt wide (a little over 80 mono
/// columns), so 80x24 matches what fits without a resize dance.
const COLS: u16 = 80;
const ROWS: u16 = 24;

/// A live shell's side of a pseudo-terminal: the master fd, the shell holding the slave.
/// The reader thread in [`Pty::spawn`] owns the master until EOF, so the fd is closed (by
/// `Drop, once the last `Arc` goes) only after the reader is done, or as the app exits.
pub struct Pty {
    fd: i32,
}

impl Pty {
    /// Run the user's `$SHELL` (or `sh`) in a PTY inside `dir`, streaming its output as
    /// `on_out` chunks and calling `on_exit` when the master hits EOF (the shell exited).
    pub fn spawn(
        dir: &str,
        on_out: impl Fn(String) + Send + 'static,
        on_exit: impl Fn() + Send + 'static,
    ) -> io::Result<Arc<Pty>> {
        let mut master: libc::c_int = 0;
        let mut slave: libc::c_int = 0;
        let ws = libc::winsize {
            ws_row: ROWS,
            ws_col: COLS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty writes two fds out, owns no other state we touch.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &ws,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // The slave's first dup becomes the child's stdio; the original is kept only to be
        // closed here (dropping it in the parent is what makes reads see EOF when the shell
        // exits).
        let slave_fd = unsafe { OwnedFd::from_raw_fd(slave) };
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());
        let mut cmd = {
            use std::os::unix::process::CommandExt;
            let mut c = std::process::Command::new(&shell);
            c.current_dir(dir)
                .stdin(slave_fd.try_clone()?)
                .stdout(slave_fd.try_clone()?)
                .stderr(slave_fd.try_clone()?)
                .env_remove("TERM_ORIGINAL");
            if std::env::var_os("TERM").is_none() {
                c.env("TERM", "xterm-256color");
            }
            // The child needs its own session with the slave as its controlling terminal, or
            // job control and Ctrl-C never reach it. std dup2's the stdio fds before
            // `pre_exec` runs, so fd 0 is already the slave here.
            // SAFETY: a fresh forked child: setsid and an ioctl on our own fd 0.
            unsafe {
                c.pre_exec(|| {
                    libc::setsid();
                    libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0);
                    Ok(())
                })
            };
            c
        };
        drop(slave_fd);
        let child = cmd.spawn()?;
        // Nothing holds the child's pid past this point: the shell outlives the GUI process
        // only until it touches the tty, at which point closing the master SIGHUPs it.
        drop(child);
        let pty = Arc::new(Pty { fd: master });
        let reader = pty.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // Output can split a UTF-8 character across reads; carry the tail until it is whole.
            let mut carry: Vec<u8> = Vec::new();
            loop {
                // SAFETY: reading into our own buffer from a fd we own.
                let n = unsafe {
                    libc::read(reader.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                };
                if n < 0 {
                    if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    break;
                }
                if n == 0 {
                    break; // EOF: every holder of the slave is gone.
                }
                carry.extend_from_slice(&buf[..n as usize]);
                loop {
                    match std::str::from_utf8(&carry) {
                        Ok(s) => {
                            on_out(s.to_string());
                            carry.clear();
                            break;
                        }
                        Err(e) => {
                            let ok = e.valid_up_to();
                            if ok > 0 {
                                on_out(String::from_utf8_lossy(&carry[..ok]).into_owned());
                                carry.drain(..ok);
                            } else if e.error_len().is_some() {
                                carry.remove(0); // a stray byte the shell's tty mangled: skip it
                            } else {
                                break; // an incomplete character: wait for the next read
                            }
                        }
                    }
                }
            }
            on_exit();
        });
        Ok(pty)
    }

    /// Send bytes to the shell: a keypress, as [`key_bytes`] spelled it.
    pub fn write(&self, bytes: &[u8]) {
        // SAFETY: writing our own bytes to a fd we own; a short write only drops keys typed
        // faster than the shell reads them.
        unsafe {
            libc::write(self.fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // SAFETY: closing a fd we own.
        unsafe { libc::close(self.fd) };
    }
}

/// The bytes a terminal sends for `e`, or `None` when the key belongs to the app (a shortcut,
/// a scroll key).
pub fn key_bytes(e: &KeyEvent) -> Option<Vec<u8>> {
    let m = &e.mods;
    // Ctrl plus a letter is how a terminal speaks: Ctrl-C (\x03), Ctrl-D (\x04), Ctrl-L…
    // Rune delivers it as `Command(c)` off macOS (where ⌘C stays a copy: `ctrl` is unset).
    if m.ctrl && !m.alt {
        let c = match &e.key {
            Key::Command(c) => Some(*c),
            Key::Text(t) => t.chars().next(),
            _ => None,
        };
        if let Some(c) = c {
            return ctrl_byte(c).map(|b| vec![b]);
        }
    }
    let esc = |b: &[u8]| {
        let mut out = vec![0x1b];
        out.extend_from_slice(b);
        out
    };
    let arrow = |csi: &str| -> Option<Vec<u8>> {
        // Ctrl+arrow is the word-jump chord readline expects.
        if m.ctrl {
            Some(format!("\x1b[1;5{csi}").into_bytes())
        } else if m.alt {
            Some(esc(&format!("[{csi}").into_bytes()))
        } else {
            Some(format!("\x1b[{csi}").into_bytes())
        }
    };
    match &e.key {
        Key::Text(t) => {
            if t == "\r" || t == "\n" {
                return Some(b"\r".to_vec());
            }
            // Option/Alt prefixes a byte with ESC, the terminal's meta key.
            (!m.cmd && !m.ctrl && !m.alt)
                .then(|| t.as_bytes().to_vec())
                .or_else(|| m.alt.then(|| esc(t.as_bytes())))
        }
        Key::Enter => Some(b"\r".to_vec()),
        Key::Backspace => Some(if m.alt {
            esc(b"\x7f")
        } else {
            b"\x7f".to_vec()
        }),
        Key::Delete => Some(b"\x1b[3~".to_vec()),
        Key::Tab => Some(if m.shift {
            b"\x1b[Z".to_vec()
        } else {
            b"\t".to_vec()
        }),
        Key::Escape => Some(b"\x1b".to_vec()),
        Key::Left => arrow("D"),
        Key::Right => arrow("C"),
        Key::Up => arrow("A"),
        Key::Down => arrow("B"),
        Key::Home => Some(b"\x1b[H".to_vec()),
        Key::End => Some(b"\x1b[F".to_vec()),
        // Page keys scroll the pane's scrollback instead, and everything else (function
        // keys, ⌘-chords the app may own) is not the shell's.
        _ => None,
    }
}

/// The control code a terminal sends for Ctrl plus `c`, or `None` for keys it does not have.
fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c.to_ascii_lowercase() {
        '@' | ' ' => 0,
        'a'..='z' => c as u8 - b'a' + 1,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        _ => return None,
    })
}

/// What the escape parser is mid-way through.
#[derive(Default)]
enum St {
    #[default]
    Ground,
    /// After ESC.
    Esc,
    /// Inside a CSI sequence: its parameters, until the final byte.
    Csi(String),
    /// Inside an OSC (window title) string, which ends at BEL.
    Osc,
    /// An ESC inside an OSC: `ESC \` ends it.
    OscEsc,
    /// A charset selection (`ESC ( B`): the next char is discarded.
    Charset,
}

/// A minimal terminal line renderer: a scroll of lines plus the line being written at
/// `col`, which is all a shell prompt and command output need. Colours, cursor addressing
/// and other full-screen sequences are consumed silently; `\r`, backspace, tab,
/// erase-in-line (`ESC [ K`) and clear-screen (`ESC [ 2 J`) are honoured, which is what
/// readline redraws and the `clear` command use.
#[derive(Default)]
pub struct TermScreen {
    lines: Vec<String>,
    /// The line under the cursor (never committed to `lines`).
    cur: Vec<char>,
    col: usize,
    st: St,
}

impl TermScreen {
    /// Render `chunk` (already whole UTF-8) into the screen.
    pub fn feed(&mut self, chunk: &str) {
        for c in chunk.chars() {
            match std::mem::replace(&mut self.st, St::Ground) {
                St::Ground => self.ground(c),
                St::Esc => self.esc(c),
                St::Csi(mut params) => {
                    if c.is_ascii_digit() || matches!(c, ';' | '?' | ':' | '<' | '=' | '>') {
                        params.push(c);
                        self.st = St::Csi(params);
                    } else {
                        self.csi(&params, c);
                    }
                }
                St::Osc => {
                    if c == '\x07' {
                        // ends the title string
                    } else if c == '\x1b' {
                        self.st = St::OscEsc;
                    } else {
                        self.st = St::Osc;
                    }
                }
                St::OscEsc => {
                    if c != '\\' {
                        self.st = St::Osc;
                    }
                }
                St::Charset => {} // the charset designator itself: discarded
            }
        }
    }

    fn ground(&mut self, c: char) {
        match c {
            '\n' => {
                let line: String = self.cur.drain(..).collect();
                self.lines.push(line);
                if self.lines.len() > super::state::TERM_LINES {
                    let cut = self.lines.len() - super::state::TERM_LINES;
                    self.lines.drain(..cut);
                }
                self.col = 0;
            }
            '\r' => self.col = 0,
            '\x08' => self.col = self.col.saturating_sub(1),
            '\t' => {
                let stop = (self.col / 8 + 1) * 8;
                while self.cur.len() < stop {
                    self.cur.push(' ');
                }
                self.col = stop;
            }
            '\x07' => {} // a bell: nothing to draw
            '\x1b' => self.st = St::Esc,
            c if (c as u32) < 0x20 => {} // other control codes: nothing to draw
            c => {
                while self.cur.len() < self.col {
                    self.cur.push(' ');
                }
                if self.col < self.cur.len() {
                    self.cur[self.col] = c;
                } else {
                    self.cur.push(c);
                }
                self.col += 1;
            }
        }
    }

    fn esc(&mut self, c: char) {
        self.st = match c {
            '[' => St::Csi(String::new()),
            ']' => St::Osc,
            '(' | ')' => St::Charset,
            _ => St::Ground, // a single-char escape (ESC 7, ESC M, …): nothing to draw
        };
    }

    /// A CSI sequence's final byte: only the erase/clear ones touch the screen.
    fn csi(&mut self, params: &str, end: char) {
        let n = params
            .trim_start_matches('?')
            .split(';')
            .next()
            .unwrap_or("");
        let n = n.parse::<usize>().unwrap_or(0);
        match end {
            'K' => match n {
                0 => self.cur.truncate(self.col), // erase to the end of the line
                1 => {
                    // erase back to the column: blanks, then the cursor sits at the start
                    for c in self.cur.iter_mut().take(self.col + 1) {
                        *c = ' ';
                    }
                }
                _ => {
                    self.cur.clear();
                    self.col = 0;
                }
            },
            'J' if n >= 2 => {
                // `clear`: the whole screen
                self.lines.clear();
                self.cur.clear();
                self.col = 0;
            }
            _ => {} // cursor moves, colours, modes: nothing a line renderer can draw
        }
    }

    /// The committed scrollback (the render adds the line under the cursor).
    #[cfg(test)]
    fn lines(&self) -> &Vec<String> {
        &self.lines
    }

    /// The screen as lines: the committed scrollback plus the line under the cursor.
    pub fn rows(&self) -> Vec<String> {
        let mut out = self.lines.clone();
        out.push(self.cur.iter().collect());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(s: &str) -> Vec<String> {
        let mut t = TermScreen::default();
        t.feed(s);
        t.rows()
    }

    #[test]
    fn plain_output_is_lines() {
        assert_eq!(screen("a\nb\r\n"), ["a", "b", ""]);
    }

    #[test]
    fn carriage_return_overwrites_the_line() {
        // A progress counter: `1` is replaced by `2` on the same line.
        assert_eq!(screen("1%\r2%"), ["2%"]);
    }

    #[test]
    fn backspace_and_tab_edit_the_line() {
        assert_eq!(screen("ab\x08c"), ["ac"]);
        assert_eq!(screen("a\tb"), ["a       b"]);
    }

    #[test]
    fn colours_and_titles_are_stripped() {
        assert_eq!(screen("\x1b[1;32mok\x1b[0m\n"), ["ok", ""]);
        assert_eq!(screen("\x1b]0;title\x07ls\r\n"), ["ls", ""]);
        assert_eq!(screen("\x1b]0;title\x1b\\ls\r\n"), ["ls", ""]);
    }

    #[test]
    fn erase_in_line_truncates_and_clear_clears() {
        // readline redraws a shorter line with ESC [ K; `clear` empties the screen.
        assert_eq!(screen("abc\x1b[K"), ["abc"]);
        assert_eq!(screen("old\r\x1b[2Knew"), ["new"]);
        assert_eq!(screen("x\n\x1b[2Jy"), ["y"]);
    }

    #[test]
    fn line_cap_keeps_the_tail() {
        let mut t = TermScreen::default();
        for i in 0..super::super::state::TERM_LINES + 10 {
            t.feed(&format!("{i}\n"));
        }
        assert_eq!(t.lines().len(), super::super::state::TERM_LINES);
        assert_eq!(t.lines().first().map(String::as_str), Some("10"));
    }

    #[test]
    fn keys_spell_terminal_bytes() {
        let k = |key: Key, ctrl: bool| KeyEvent {
            key,
            mods: rune::Mods {
                ctrl,
                ..rune::Mods::default()
            },
        };
        assert_eq!(
            key_bytes(&k(Key::Text("ls".into()), false)),
            Some(b"ls".to_vec())
        );
        assert_eq!(key_bytes(&k(Key::Enter, false)), Some(b"\r".to_vec()));
        assert_eq!(key_bytes(&k(Key::Backspace, false)), Some(b"\x7f".to_vec()));
        assert_eq!(key_bytes(&k(Key::Up, false)), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes(&k(Key::Up, true)), Some(b"\x1b[1;5A".to_vec()));
        assert_eq!(
            key_bytes(&k(Key::Command('c'), true)),
            Some(b"\x03".to_vec())
        );
        assert_eq!(
            key_bytes(&k(Key::Command('d'), true)),
            Some(b"\x04".to_vec())
        );
        assert_eq!(
            key_bytes(&k(Key::Command('c'), false)),
            None,
            "a bare ⌘C copies"
        );
        assert_eq!(
            key_bytes(&k(Key::PageUp, false)),
            None,
            "the pane scrolls instead"
        );
    }

    /// A real shell in a real PTY: `echo` writes back through the tty, and the exit closes it.
    #[test]
    fn a_shell_round_trips_through_the_pty() {
        let dir = std::env::temp_dir().join(format!("dex-gui-pty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let ex = std::sync::Arc::new(std::sync::Mutex::new(false));
        let (o, e2) = (out.clone(), ex.clone());
        let pty = Pty::spawn(
            dir.to_str().unwrap(),
            move |s| o.lock().unwrap().push_str(&s),
            move || *e2.lock().unwrap() = true,
        )
        .expect("openpty");
        // Keep every keystroke a plain byte so no readline of the host's $SHELL can swallow it.
        for b in b"printf dex-term-probe; exit\n" {
            pty.write(&[*b]);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !*ex.lock().unwrap() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(*ex.lock().unwrap(), "the shell exited");
        assert!(
            out.lock().unwrap().contains("dex-term-probe"),
            "the tty echoed the command"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
