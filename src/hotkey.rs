use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    UnregisterHotKey,
};
use windows::core::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HotKeyBinding {
    modifiers: HOT_KEY_MODIFIERS,
    virtual_key: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedHotKey {
    pub(crate) binding: HotKeyBinding,
    pub(crate) display: String,
}

fn add_modifier(
    modifiers: &mut HOT_KEY_MODIFIERS,
    modifier: HOT_KEY_MODIFIERS,
    name: &str,
) -> std::result::Result<(), String> {
    if modifiers.contains(modifier) {
        return Err(format!("修饰键 {name} 重复"));
    }
    *modifiers |= modifier;
    Ok(())
}

fn parse_virtual_key(token: &str) -> Option<(u32, String)> {
    let upper = token.to_ascii_uppercase();
    if upper.len() == 1 {
        let key = upper.as_bytes()[0];
        if key.is_ascii_alphanumeric() {
            return Some((key as u32, upper));
        }
    }
    let number = upper.strip_prefix('F')?.parse::<u32>().ok()?;
    (1..=24)
        .contains(&number)
        .then(|| (0x70 + number - 1, format!("F{number}")))
}

/// 解析用户可读的全局热键，例如 `Ctrl+Alt+Z`。空字符串表示禁用。
pub(crate) fn parse_hotkey(input: &str) -> std::result::Result<Option<ParsedHotKey>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }

    let mut modifiers = HOT_KEY_MODIFIERS::default();
    let mut key = None;
    for raw in input.split('+') {
        let token = raw.trim();
        if token.is_empty() {
            return Err("快捷键中存在空白按键".into());
        }
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => add_modifier(&mut modifiers, MOD_CONTROL, "Ctrl")?,
            "alt" => add_modifier(&mut modifiers, MOD_ALT, "Alt")?,
            "shift" => add_modifier(&mut modifiers, MOD_SHIFT, "Shift")?,
            "win" | "windows" => add_modifier(&mut modifiers, MOD_WIN, "Win")?,
            _ => {
                if key.is_some() {
                    return Err("快捷键只能包含一个普通按键".into());
                }
                key = parse_virtual_key(token);
                if key.is_none() {
                    return Err(format!("不支持按键“{token}”；请使用 A-Z、0-9 或 F1-F24"));
                }
            }
        }
    }

    if modifiers.0 == 0 {
        return Err("快捷键至少需要 Ctrl、Alt、Shift 或 Win 中的一个修饰键".into());
    }
    let Some((virtual_key, key_name)) = key else {
        return Err("快捷键缺少普通按键".into());
    };
    let mut names = Vec::new();
    for (modifier, name) in [
        (MOD_CONTROL, "Ctrl"),
        (MOD_ALT, "Alt"),
        (MOD_SHIFT, "Shift"),
        (MOD_WIN, "Win"),
    ] {
        if modifiers.contains(modifier) {
            names.push(name);
        }
    }
    names.push(&key_name);

    Ok(Some(ParsedHotKey {
        binding: HotKeyBinding {
            modifiers: modifiers | MOD_NOREPEAT,
            virtual_key,
        },
        display: names.join("+"),
    }))
}

/// 一次与指定窗口和 ID 绑定的全局热键注册。
pub(crate) struct RegisteredHotKey {
    hwnd: HWND,
    id: i32,
    binding: HotKeyBinding,
}

impl RegisteredHotKey {
    pub(crate) fn register(hwnd: HWND, id: i32, binding: HotKeyBinding) -> Result<Self> {
        unsafe { RegisterHotKey(Some(hwnd), id, binding.modifiers, binding.virtual_key)? };
        Ok(Self { hwnd, id, binding })
    }

    pub(crate) fn id(&self) -> i32 {
        self.id
    }

    pub(crate) fn binding(&self) -> HotKeyBinding {
        self.binding
    }
}

impl Drop for RegisteredHotKey {
    fn drop(&mut self) {
        let _ = unsafe { UnregisterHotKey(Some(self.hwnd), self.id) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_F24;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
    };
    use windows::core::{PCWSTR, w};

    struct WindowGuard(HWND);

    impl Drop for WindowGuard {
        fn drop(&mut self) {
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    #[test]
    fn registered_hotkey_releases_the_combination_when_dropped() {
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                32,
                32,
                None,
                None,
                None,
                None,
            )
        }
        .expect("test window should be created");
        let _window = WindowGuard(hwnd);
        let binding = HotKeyBinding {
            modifiers: MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT,
            virtual_key: VK_F24.0 as u32,
        };

        let registration = RegisteredHotKey::register(hwnd, 1001, binding)
            .expect("first registration should succeed");
        assert!(RegisteredHotKey::register(hwnd, 1002, binding).is_err());

        drop(registration);

        let replacement = RegisteredHotKey::register(hwnd, 1002, binding)
            .expect("dropping the owner should unregister the hotkey");
        drop(replacement);
    }

    #[test]
    fn parses_and_normalizes_supported_hotkeys() {
        let parsed = parse_hotkey(" shift + ctrl + f8 ").unwrap().unwrap();
        assert_eq!(parsed.display, "Ctrl+Shift+F8");
        assert_eq!(parsed.binding.virtual_key, 0x77);
        assert!(parsed.binding.modifiers.contains(MOD_CONTROL));
        assert!(parsed.binding.modifiers.contains(MOD_SHIFT));
        assert!(parsed.binding.modifiers.contains(MOD_NOREPEAT));
    }

    #[test]
    fn blank_hotkey_disables_registration() {
        assert_eq!(parse_hotkey("  ").unwrap(), None);
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_hotkeys() {
        assert!(parse_hotkey("Z").is_err());
        assert!(parse_hotkey("Ctrl+Alt").is_err());
        assert!(parse_hotkey("Ctrl+Alt+Z+X").is_err());
        assert!(parse_hotkey("Ctrl+Ctrl+Z").is_err());
        assert!(parse_hotkey("Ctrl+Alt+Escape").is_err());
    }
}
