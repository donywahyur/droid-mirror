//! Serialization of scrcpy control messages (client -> device).
//! Wire format matches scrcpy-server 5.0.1 `ControlMessageReader`. All integers are big-endian.

pub const TYPE_INJECT_KEYCODE: u8 = 0;
pub const TYPE_INJECT_TEXT: u8 = 1;
pub const TYPE_INJECT_TOUCH_EVENT: u8 = 2;
pub const TYPE_INJECT_SCROLL_EVENT: u8 = 3;
pub const TYPE_EXPAND_NOTIFICATION_PANEL: u8 = 5;
pub const TYPE_GET_CLIPBOARD: u8 = 8;
pub const TYPE_SET_CLIPBOARD: u8 = 9;
pub const TYPE_SET_DISPLAY_POWER: u8 = 10;
pub const TYPE_ROTATE_DEVICE: u8 = 11;
pub const TYPE_RESET_VIDEO: u8 = 17;
pub const TYPE_SCAN_FILE: u8 = 22;

/// Touch events with this pointer id are injected as a finger (not a mouse).
pub const POINTER_ID_GENERIC_FINGER: u64 = -2i64 as u64;

pub const INJECT_TEXT_MAX_BYTES: usize = 300;

pub const COPY_KEY_COPY: u8 = 1;

fn u16_fixed_point(f: f32) -> u16 {
    let v = (f.clamp(0.0, 1.0) * 65536.0) as u32;
    v.min(0xffff) as u16
}

fn i16_fixed_point(f: f32) -> i16 {
    let v = (f.clamp(-1.0, 1.0) * 32768.0) as i32;
    v.clamp(-0x8000, 0x7fff) as i16
}

pub fn keycode(action: u8, keycode: u32, repeat: u32, meta_state: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(14);
    m.push(TYPE_INJECT_KEYCODE);
    m.push(action);
    m.extend_from_slice(&keycode.to_be_bytes());
    m.extend_from_slice(&repeat.to_be_bytes());
    m.extend_from_slice(&meta_state.to_be_bytes());
    m
}

/// Cut `text` into chunks that fit the server's inject-text limit, on char boundaries.
pub fn text_chunks(text: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + INJECT_TEXT_MAX_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

pub fn text(chunk: &str) -> Vec<u8> {
    let bytes = chunk.as_bytes();
    let mut m = Vec::with_capacity(5 + bytes.len());
    m.push(TYPE_INJECT_TEXT);
    m.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    m.extend_from_slice(bytes);
    m
}

fn position(m: &mut Vec<u8>, x: i32, y: i32, w: u16, h: u16) {
    m.extend_from_slice(&x.to_be_bytes());
    m.extend_from_slice(&y.to_be_bytes());
    m.extend_from_slice(&w.to_be_bytes());
    m.extend_from_slice(&h.to_be_bytes());
}

/// `action`: 0 = down, 1 = up, 2 = move (Android MotionEvent actions).
pub fn touch(action: u8, x: i32, y: i32, w: u16, h: u16) -> Vec<u8> {
    let pressure = if action == 1 { 0.0 } else { 1.0 };
    let mut m = Vec::with_capacity(32);
    m.push(TYPE_INJECT_TOUCH_EVENT);
    m.push(action);
    m.extend_from_slice(&POINTER_ID_GENERIC_FINGER.to_be_bytes());
    position(&mut m, x, y, w, h);
    m.extend_from_slice(&u16_fixed_point(pressure).to_be_bytes());
    m.extend_from_slice(&0u32.to_be_bytes()); // action button
    m.extend_from_slice(&0u32.to_be_bytes()); // buttons
    m
}

/// Scroll amounts are in "notches"; the server accepts the range [-16, 16].
pub fn scroll(x: i32, y: i32, w: u16, h: u16, hscroll: f32, vscroll: f32) -> Vec<u8> {
    let mut m = Vec::with_capacity(21);
    m.push(TYPE_INJECT_SCROLL_EVENT);
    position(&mut m, x, y, w, h);
    m.extend_from_slice(&i16_fixed_point(hscroll / 16.0).to_be_bytes());
    m.extend_from_slice(&i16_fixed_point(vscroll / 16.0).to_be_bytes());
    m.extend_from_slice(&0u32.to_be_bytes()); // buttons
    m
}

pub fn get_clipboard(copy_key: u8) -> Vec<u8> {
    vec![TYPE_GET_CLIPBOARD, copy_key]
}

pub fn set_clipboard(sequence: u64, text: &str, paste: bool) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut m = Vec::with_capacity(14 + bytes.len());
    m.push(TYPE_SET_CLIPBOARD);
    m.extend_from_slice(&sequence.to_be_bytes());
    m.push(paste as u8);
    m.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    m.extend_from_slice(bytes);
    m
}

/// Turn the phone's physical display on/off. Mirroring and input keep working.
pub fn display_power(on: bool) -> Vec<u8> {
    vec![TYPE_SET_DISPLAY_POWER, on as u8]
}

/// Ask Android's media scanner to index a file, so it shows up in the gallery.
pub fn scan_file(path: &str) -> Vec<u8> {
    let bytes = path.as_bytes();
    let mut m = Vec::with_capacity(5 + bytes.len());
    m.push(TYPE_SCAN_FILE);
    m.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    m.extend_from_slice(bytes);
    m
}

pub fn empty(kind: u8) -> Vec<u8> {
    vec![kind]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_layout() {
        let m = touch(0, 100, 200, 1080, 1920);
        assert_eq!(m.len(), 32);
        assert_eq!(m[0], TYPE_INJECT_TOUCH_EVENT);
        assert_eq!(&m[2..10], &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe]);
        assert_eq!(&m[10..14], &100i32.to_be_bytes());
        assert_eq!(&m[18..20], &1080u16.to_be_bytes());
        assert_eq!(&m[22..24], &[0xff, 0xff]); // pressure 1.0
    }

    #[test]
    fn keycode_layout() {
        assert_eq!(keycode(1, 4, 0, 0), vec![0, 1, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn scan_file_layout() {
        assert_eq!(scan_file("/a"), vec![22, 0, 0, 0, 2, b'/', b'a']);
    }

    #[test]
    fn display_power_layout() {
        assert_eq!(display_power(false), vec![10, 0]);
        assert_eq!(display_power(true), vec![10, 1]);
    }

    #[test]
    fn scroll_layout() {
        let m = scroll(1, 2, 3, 4, 0.0, -16.0);
        assert_eq!(m.len(), 21);
        assert_eq!(&m[15..17], &[0x80, 0x00]);
    }

    #[test]
    fn text_chunking_respects_utf8() {
        let s = "é".repeat(200); // 400 bytes
        let chunks = text_chunks(&s);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.len() <= INJECT_TEXT_MAX_BYTES));
        assert_eq!(chunks.concat(), s);
    }
}
