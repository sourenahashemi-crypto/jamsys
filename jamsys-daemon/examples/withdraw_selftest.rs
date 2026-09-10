//! Raise a real notification through the daemon's own client, then withdraw it.
//!
//! Proves the resolve path actually takes the notification off the screen, which
//! is the half that was missing: the id was dropped and the shell kept showing it.
fn main() {
    let mut conn = match jamsys::dbus::Connection::session() {
        Ok(c) => c,
        Err(e) => { eprintln!("no session bus: {e}"); return; }
    };
    let id = jamsys::dbus::notify(
        &mut conn, "JamSys", 0, "dialog-warning",
        "JamSys withdraw self-test",
        "This should vanish on its own about three seconds from now.",
        2, 0,   // critical urgency, never expires -- the worst case
    ).expect("notify");
    println!("raised notification id={id} (urgency=critical, timeout=never)");
    std::thread::sleep(std::time::Duration::from_secs(3));
    jamsys::dbus::close_notification(&mut conn, id).expect("close");
    println!("withdrew notification id={id}");
}
