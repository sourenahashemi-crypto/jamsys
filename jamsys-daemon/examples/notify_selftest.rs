fn main() {
    match jamsys::dbus::Connection::session() {
        Ok(mut c) => {
            println!("session bus connected as {}", c.unique_name);
            match jamsys::dbus::notify(
                &mut c, "JamSys", 0, "dialog-warning",
                "JamSys self-test",
                "This notification was produced by the daemon's own D-Bus client.\nCPU package temperature is 96.4 °C. Expected: below 95 °C.",
                1, 8000) {
                Ok(id) => println!("NOTIFICATION DELIVERED, id={id}"),
                Err(e) => println!("notify failed: {e}"),
            }
        }
        Err(e) => println!("no session bus: {e}"),
    }
}
