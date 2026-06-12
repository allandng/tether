//! Live keyboard verification: inject "tether" + Shift+A + Cmd+S through the
//! real injector into whatever app is frontmost. Pair with a TextEdit window
//! on a temp file and read the file back to verify, including modifiers.
//!
//! Run: cargo run -p tetherd --example inject_typing

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use std::{thread::sleep, time::Duration};
    use tetherd::input::InputInjector;
    use tetherd::input::macos::MacInjector;
    use tether_protocol::{InputEvent, modifiers};

    fn tap(injector: &mut MacInjector, code: &str, mods: u8) -> anyhow::Result<()> {
        injector.inject(&InputEvent::KeyDown { code: code.into(), modifiers: mods })?;
        sleep(Duration::from_millis(15));
        injector.inject(&InputEvent::KeyUp { code: code.into(), modifiers: mods })?;
        sleep(Duration::from_millis(25));
        Ok(())
    }

    let mut injector = MacInjector::new()?;
    for code in ["KeyT", "KeyE", "KeyT", "KeyH", "KeyE", "KeyR"] {
        tap(&mut injector, code, 0)?;
    }

    // Shift+A — modifier key travels as its own event, like the browser sends it
    injector.inject(&InputEvent::KeyDown { code: "ShiftLeft".into(), modifiers: modifiers::SHIFT })?;
    sleep(Duration::from_millis(25));
    tap(&mut injector, "KeyA", modifiers::SHIFT)?;
    injector.inject(&InputEvent::KeyUp { code: "ShiftLeft".into(), modifiers: 0 })?;
    sleep(Duration::from_millis(25));

    // Cmd+S
    injector.inject(&InputEvent::KeyDown { code: "MetaLeft".into(), modifiers: modifiers::META })?;
    sleep(Duration::from_millis(25));
    tap(&mut injector, "KeyS", modifiers::META)?;
    injector.inject(&InputEvent::KeyUp { code: "MetaLeft".into(), modifiers: 0 })?;

    println!("injected: tether, Shift+A, Cmd+S");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("inject_typing is macOS-only");
}
