//! Client-side line editing for high-latency SSH connections.

const ALT_ENTER: &[u8] = b"\x1bP+ssht;alt=1\x1b\\";
const ALT_EXIT: &[u8] = b"\x1bP+ssht;alt=0\x1b\\";
const PASSWORD_HINTS: [&[u8]; 4] = [b"assword", b"assphrase", b"PIN", b"Verification code"];
const TOGGLE_KEY: u8 = 0x1d; // Ctrl-]
const BAILOUT_KEYS: [u8; 11] = [
    0x09, 0x12, 0x1b, 0x01, 0x05, 0x02, 0x06, 0x0b, 0x10, 0x0e, 0x0c,
];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RelayAction {
    pub to_remote: Vec<u8>,
    pub to_terminal: Vec<u8>,
}

#[derive(Debug)]
pub struct LocalEcho {
    enabled: bool,
    alt_screen: bool,
    suspended: bool,
    disarm_next_line: bool,
    line: Vec<u8>,
    pending_echo: Vec<u8>,
    scan_tail: Vec<u8>,
    marker_buf: Vec<u8>,
}

impl LocalEcho {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            alt_screen: false,
            suspended: false,
            disarm_next_line: false,
            line: Vec::new(),
            pending_echo: Vec::new(),
            scan_tail: Vec::new(),
            marker_buf: Vec::new(),
        }
    }

    fn line_mode(&self) -> bool {
        self.enabled && !self.alt_screen && !self.suspended && !self.disarm_next_line
    }

    fn flush_line(&mut self, out: &mut Vec<u8>) {
        if !self.line.is_empty() {
            out.extend_from_slice(&self.line);
            self.pending_echo.extend_from_slice(&self.line);
            self.line.clear();
        }
    }

    pub fn on_output(&mut self, chunk: &[u8]) -> RelayAction {
        let mut action = RelayAction::default();
        let visible = self.filter_alt_markers(chunk);
        let mut visible = visible.as_slice();

        if !self.pending_echo.is_empty() {
            let matched = visible
                .iter()
                .zip(self.pending_echo.iter())
                .take_while(|(actual, expected)| actual == expected)
                .count();
            self.pending_echo.drain(..matched);
            visible = &visible[matched..];
            if !visible.is_empty() && !self.pending_echo.is_empty() {
                self.pending_echo.clear();
            }
        }

        let old_len = self.scan_tail.len();
        let mut scan = std::mem::take(&mut self.scan_tail);
        scan.extend_from_slice(visible);

        if PASSWORD_HINTS
            .iter()
            .any(|hint| has_new_match(&scan, old_len, hint))
        {
            if !self.line.is_empty() {
                self.line.clear();
                action.to_terminal.extend_from_slice(b"\r\x1b[2K");
            }
            self.disarm_next_line = true;
        }

        let carry = PASSWORD_HINTS
            .iter()
            .map(|sequence| sequence.len())
            .max()
            .unwrap_or(1)
            - 1;
        self.scan_tail = scan[scan.len().saturating_sub(carry)..].to_vec();
        action.to_terminal.extend_from_slice(visible);
        action
    }

    fn filter_alt_markers(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut visible = Vec::with_capacity(chunk.len());
        for &byte in chunk {
            if self.marker_buf.is_empty() && byte != 0x1b {
                visible.push(byte);
                continue;
            }

            self.marker_buf.push(byte);
            if ALT_ENTER.starts_with(&self.marker_buf) || ALT_EXIT.starts_with(&self.marker_buf) {
                if self.marker_buf == ALT_ENTER {
                    if !self.alt_screen {
                        self.alt_screen = true;
                        // The pane changed independently; buffered text was
                        // intended for the shell and must not become app keys.
                        self.line.clear();
                    }
                    self.marker_buf.clear();
                } else if self.marker_buf == ALT_EXIT {
                    self.alt_screen = false;
                    self.marker_buf.clear();
                }
                continue;
            }

            visible.append(&mut self.marker_buf);
        }
        visible
    }

    pub fn on_input(&mut self, data: &[u8]) -> RelayAction {
        let mut action = RelayAction::default();
        for &byte in data {
            if byte == TOGGLE_KEY {
                self.flush_line(&mut action.to_remote);
                self.enabled = !self.enabled;
                let state = if self.enabled {
                    b"local line editing ON".as_slice()
                } else {
                    b"local line editing OFF (raw passthrough)".as_slice()
                };
                action.to_terminal.extend_from_slice(b"\r\n\x1b[7m ");
                action.to_terminal.extend_from_slice(state);
                action.to_terminal.extend_from_slice(b" \x1b[0m\r\n");
                continue;
            }

            if !self.line_mode() {
                if matches!(byte, b'\r' | b'\n') {
                    self.suspended = false;
                    self.disarm_next_line = false;
                }
                action.to_remote.push(byte);
                continue;
            }

            match byte {
                b'\r' | b'\n' => {
                    self.flush_line(&mut action.to_remote);
                    action.to_remote.push(b'\r');
                }
                0x7f | 0x08 => {
                    if pop_utf8_char(&mut self.line) {
                        action.to_terminal.extend_from_slice(b"\x08 \x08");
                    }
                }
                0x15 => {
                    let chars = utf8_char_count(&self.line);
                    self.line.clear();
                    action.to_terminal.extend(
                        std::iter::repeat_n(b"\x08 \x08".as_slice(), chars)
                            .flatten()
                            .copied(),
                    );
                }
                0x17 => {
                    let mut chars = 0;
                    while self.line.last() == Some(&b' ') {
                        self.line.pop();
                        chars += 1;
                    }
                    while !self.line.is_empty() && self.line.last() != Some(&b' ') {
                        pop_utf8_char(&mut self.line);
                        chars += 1;
                    }
                    action.to_terminal.extend(
                        std::iter::repeat_n(b"\x08 \x08".as_slice(), chars)
                            .flatten()
                            .copied(),
                    );
                }
                0x03 => {
                    self.line.clear();
                    action.to_remote.push(byte);
                }
                0x04 => {
                    self.flush_line(&mut action.to_remote);
                    self.suspended = true;
                    action.to_remote.push(byte);
                }
                byte if BAILOUT_KEYS.contains(&byte) || byte < 0x20 => {
                    self.flush_line(&mut action.to_remote);
                    self.suspended = true;
                    action.to_remote.push(byte);
                }
                _ => {
                    self.line.push(byte);
                    action.to_terminal.push(byte);
                }
            }
        }
        action
    }
}

fn has_new_match(haystack: &[u8], old_len: usize, needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .enumerate()
        .any(|(index, window)| window == needle && index + needle.len() > old_len)
}

fn pop_utf8_char(line: &mut Vec<u8>) -> bool {
    if line.is_empty() {
        return false;
    }
    while line.len() > 1 && line.last().is_some_and(|byte| byte & 0xc0 == 0x80) {
        line.pop();
    }
    line.pop();
    true
}

fn utf8_char_count(line: &[u8]) -> usize {
    line.iter().filter(|byte| **byte & 0xc0 != 0x80).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_and_draws_a_line_then_suppresses_its_echo() {
        let mut editor = LocalEcho::new(true);
        assert_eq!(editor.on_input(b"hello").to_terminal, b"hello");
        let commit = editor.on_input(b"\r");
        assert_eq!(commit.to_remote, b"hello\r");
        assert_eq!(editor.on_output(b"hello\r\n$ ").to_terminal, b"\r\n$ ");
    }

    #[test]
    fn echo_mismatch_self_heals() {
        let mut editor = LocalEcho::new(true);
        editor.on_input(b"abc\r");
        assert_eq!(editor.on_output(b"\x1b[31mabc").to_terminal, b"\x1b[31mabc");
        assert_eq!(editor.on_output(b"more").to_terminal, b"more");
    }

    #[test]
    fn alt_screen_sequence_can_span_reads() {
        let mut editor = LocalEcho::new(true);
        editor.on_output(b"\x1bP+ssht;");
        editor.on_output(b"alt=1\x1b\\");
        assert_eq!(editor.on_input(b"x").to_remote, b"x");
        editor.on_output(ALT_EXIT);
        assert_eq!(editor.on_input(b"y").to_terminal, b"y");
    }

    #[test]
    fn alt_screen_entry_discards_unsent_shell_input() {
        let mut editor = LocalEcho::new(true);
        editor.on_input(b"shell input");
        assert!(editor.on_output(ALT_ENTER).to_remote.is_empty());
        editor.on_output(ALT_EXIT);
        assert_eq!(editor.on_input(b"\r").to_remote, b"\r");
    }

    #[test]
    fn tmux_outer_alt_screen_does_not_disable_line_editing() {
        let mut editor = LocalEcho::new(true);
        assert_eq!(editor.on_output(b"\x1b[?1049h").to_terminal, b"\x1b[?1049h");
        assert_eq!(editor.on_input(b"shell").to_terminal, b"shell");
    }

    #[test]
    fn bailout_flushes_and_rearms_after_enter() {
        let mut editor = LocalEcho::new(true);
        editor.on_input(b"car");
        assert_eq!(editor.on_input(b"\t").to_remote, b"car\t");
        assert_eq!(editor.on_input(b"go\r").to_remote, b"go\r");
        assert_eq!(editor.on_input(b"next").to_terminal, b"next");
    }

    #[test]
    fn password_hint_can_span_reads_and_only_disarms_one_line() {
        let mut editor = LocalEcho::new(true);
        editor.on_output(b"Passw");
        editor.on_output(b"ord: ");
        assert_eq!(editor.on_input(b"secret\r").to_remote, b"secret\r");
        assert!(editor.on_input(b"visible").to_remote.is_empty());
    }

    #[test]
    fn password_hint_discards_input_typed_before_the_prompt_arrived() {
        let mut editor = LocalEcho::new(true);
        editor.on_input(b"oops");
        let prompt = editor.on_output(b"Password: ");
        assert!(prompt.to_terminal.starts_with(b"\r\x1b[2K"));
        assert_eq!(editor.on_input(b"secret\r").to_remote, b"secret\r");
        assert!(editor.on_input(b"next\r").to_remote.ends_with(b"\r"));
    }

    #[test]
    fn ctrl_d_flushes_visible_input_before_forwarding() {
        let mut editor = LocalEcho::new(true);
        editor.on_input(b"text");
        assert_eq!(editor.on_input(b"\x04").to_remote, b"text\x04");
    }

    #[test]
    fn backspace_erases_one_multibyte_character() {
        let mut editor = LocalEcho::new(true);
        editor.on_input("aé🎉".as_bytes());
        editor.on_input(b"\x7f");
        editor.on_input(b"\x7f");
        assert_eq!(editor.on_input(b"\r").to_remote, b"a\r");
    }

    #[test]
    fn toggle_flushes_then_uses_passthrough() {
        let mut editor = LocalEcho::new(true);
        editor.on_input(b"abc");
        let toggle = editor.on_input(&[TOGGLE_KEY]);
        assert_eq!(toggle.to_remote, b"abc");
        assert_eq!(editor.on_input(b"x").to_remote, b"x");
    }
}
