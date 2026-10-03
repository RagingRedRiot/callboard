//! Follows the desktop's light or dark appearance.
//!
//! Desktops publish it through the freedesktop settings portal as
//! `org.freedesktop.appearance` `color-scheme`, which GNOME, KDE, COSMIC,
//! and others set from their appearance settings. The app reads it at
//! startup and follows changes while it runs. Without a portal, the app
//! keeps its default.

use eframe::egui::{self, ThemePreference};
use std::time::Duration;
use zbus::zvariant::{OwnedValue, Value};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS: &str = "org.freedesktop.portal.Settings";
const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";

/// The theme for a `color-scheme` value: 1 prefers dark, 2 prefers light,
/// and anything else leaves the choice to the app.
pub fn preference(color_scheme: u32) -> ThemePreference {
    match color_scheme {
        1 => ThemePreference::Dark,
        2 => ThemePreference::Light,
        _ => ThemePreference::System,
    }
}

/// Apply the desktop's appearance to `ctx` now, and keep following it.
pub fn follow(ctx: &egui::Context) {
    let Ok(conn) = zbus::blocking::connection::Builder::session()
        .map(|b| b.method_timeout(Duration::from_secs(1)))
        .and_then(|b| b.build())
    else {
        return;
    };
    if let Some(scheme) = read(&conn) {
        ctx.set_theme(preference(scheme));
    }
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new()
        .name("appearance".into())
        .spawn(move || watch(&conn, &ctx));
}

/// The current `color-scheme`, if the desktop publishes one.
fn read(conn: &zbus::blocking::Connection) -> Option<u32> {
    // `ReadOne` returns the value; `Read`, which older portals offer
    // instead, returns it wrapped in a second variant.
    ["ReadOne", "Read"].into_iter().find_map(|method| {
        let reply = conn
            .call_method(
                Some(PORTAL),
                PATH,
                Some(SETTINGS),
                method,
                &(NAMESPACE, KEY),
            )
            .ok()?;
        let value: OwnedValue = reply.body().deserialize().ok()?;
        color_scheme(&value)
    })
}

/// Apply each change to `color-scheme` as the desktop announces it.
fn watch(conn: &zbus::blocking::Connection, ctx: &egui::Context) {
    let Ok(proxy) = zbus::blocking::Proxy::new(conn, PORTAL, PATH, SETTINGS) else {
        return;
    };
    let Ok(changes) = proxy.receive_signal("SettingChanged") else {
        return;
    };
    for message in changes {
        let Ok((namespace, key, value)) =
            message.body().deserialize::<(String, String, OwnedValue)>()
        else {
            continue;
        };
        if namespace == NAMESPACE
            && key == KEY
            && let Some(scheme) = color_scheme(&value)
        {
            ctx.set_theme(preference(scheme));
            ctx.request_repaint();
        }
    }
}

/// A `color-scheme` value, unwrapping any variants around it.
fn color_scheme(value: &Value) -> Option<u32> {
    match value {
        Value::U32(scheme) => Some(*scheme),
        Value::Value(inner) => color_scheme(inner),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_scheme_values_choose_a_theme() {
        assert_eq!(preference(1), ThemePreference::Dark);
        assert_eq!(preference(2), ThemePreference::Light);
        assert_eq!(preference(0), ThemePreference::System);
        assert_eq!(preference(7), ThemePreference::System);
    }

    #[test]
    fn values_are_read_through_variants() {
        assert_eq!(color_scheme(&Value::U32(1)), Some(1));
        let wrapped = Value::Value(Box::new(Value::Value(Box::new(Value::U32(2)))));
        assert_eq!(color_scheme(&wrapped), Some(2));
        assert_eq!(color_scheme(&Value::from("dark")), None);
    }
}
