//! Global hotkeys on Wayland through the desktop portal (Linux).
//!
//! The global-shortcut plugin grabs keys through X11, which a Wayland
//! compositor doesn't allow, so on Wayland the hotkeys go through the
//! org.freedesktop.portal.GlobalShortcuts portal when the desktop offers it
//! (KDE Plasma, GNOME 48+, Hyprland, ...). The configured keys are only the
//! preferred triggers: the desktop confirms or changes them (usually in its
//! own dialog the first time) and reports what it assigned, which the
//! settings show.

use std::{
  collections::HashMap,
  sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Mutex,
  },
};

use ashpd::{
  desktop::{
    global_shortcuts::{
      BindShortcutsOptions, ConfigureShortcutsOptions, GlobalShortcuts, NewShortcut, Shortcut,
    },
    CreateSessionOptions, Session,
  },
  AppID,
};
use futures_util::StreamExt;
use tauri::{AppHandle, Manager};

use crate::hotkeys::{Binding, BindingResult, HotkeyCommand, HotkeysState};

/// The Flatpak id: portal app ids need a dot, and a Flatpak build has the same
const APP_ID: &str = "dev.overlayed.Overlayed";

#[derive(Default)]
pub struct PortalHotkeys {
  /// the portal offers GlobalShortcuts (decided by the first `apply`)
  available: AtomicBool,
  probed: AtomicBool,
  version: AtomicU32,
  inner: tokio::sync::Mutex<Option<Inner>>,
  /// shortcut id -> the trigger the desktop assigned ("" if none)
  assigned: Mutex<HashMap<String, String>>,
}

struct Inner {
  proxy: GlobalShortcuts,
  session: Option<Session<GlobalShortcuts>>,
  /// preferred triggers of the bound session, to skip rebinding unchanged keys
  bound: Vec<(String, Option<String>)>,
}

fn id(command: HotkeyCommand) -> String {
  serde_json::to_value(command)
    .ok()
    .and_then(|value| value.as_str().map(str::to_string))
    .unwrap_or_default()
}

fn command(id: &str) -> Option<HotkeyCommand> {
  serde_json::from_value(serde_json::Value::String(id.to_string())).ok()
}

fn description(command: HotkeyCommand) -> &'static str {
  match command {
    HotkeyCommand::Pin => "Pin or unpin the overlay",
    HotkeyCommand::ToggleVisibility => "Show or hide the overlay",
    HotkeyCommand::NextMonitor => "Move the overlay to the next monitor",
    HotkeyCommand::OpenSettings => "Open the Overlayed settings",
  }
}

/// An accelerator as the settings store it ("CommandOrControl+Shift+H") to a
/// trigger in the XDG shortcuts format ("CTRL+SHIFT+h")
pub fn xdg_trigger(accelerator: &str) -> Option<String> {
  let mut parts: Vec<String> = Vec::new();
  let mut key = None;
  for token in accelerator
    .split('+')
    .map(str::trim)
    .filter(|token| !token.is_empty())
  {
    let modifier = match token.to_ascii_lowercase().as_str() {
      "commandorcontrol" | "cmdorctrl" | "commandorctrl" | "cmdorcontrol" | "control" | "ctrl" => {
        Some("CTRL")
      }
      "shift" => Some("SHIFT"),
      "alt" | "option" | "altgr" => Some("ALT"),
      "super" | "meta" | "command" | "cmd" => Some("LOGO"),
      _ => None,
    };
    match modifier {
      Some(modifier) => {
        if !parts.iter().any(|part| part == modifier) {
          parts.push(modifier.to_string());
        }
      }
      None => key = Some(keysym(token)?),
    }
  }
  parts.push(key?);
  Some(parts.join("+"))
}

/// tauri key names to XKB keysym names
fn keysym(key: &str) -> Option<String> {
  let lower = key.to_ascii_lowercase();
  let name = match lower.as_str() {
    "space" => "space",
    "enter" | "return" => "Return",
    "tab" => "Tab",
    "escape" | "esc" => "Escape",
    "backspace" => "BackSpace",
    "delete" | "del" => "Delete",
    "insert" => "Insert",
    "home" => "Home",
    "end" => "End",
    "pageup" => "Page_Up",
    "pagedown" => "Page_Down",
    "up" | "arrowup" => "Up",
    "down" | "arrowdown" => "Down",
    "left" | "arrowleft" => "Left",
    "right" | "arrowright" => "Right",
    "minus" | "-" => "minus",
    "equal" | "=" => "equal",
    "comma" | "," => "comma",
    "period" | "." => "period",
    "slash" | "/" => "slash",
    "backslash" | "\\" => "backslash",
    "semicolon" | ";" => "semicolon",
    "quote" | "'" => "apostrophe",
    "backquote" | "`" => "grave",
    "bracketleft" | "[" => "bracketleft",
    "bracketright" | "]" => "bracketright",
    _ => {
      // letters lowercase, digits as they are, F1..F24 as they are
      let key = lower.strip_prefix("key").unwrap_or(&lower);
      let key = key.strip_prefix("digit").unwrap_or(key);
      if key.len() == 1 && key.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Some(key.to_string());
      }
      if let Some(number) = key.strip_prefix('f') {
        if number.parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) {
          return Some(format!("F{number}"));
        }
      }
      return None;
    }
  };
  Some(name.to_string())
}

/// Whether hotkeys go through the portal: on Wayland, when it offers them.
pub fn in_use(app: &AppHandle) -> bool {
  app
    .state::<PortalHotkeys>()
    .available
    .load(Ordering::Acquire)
}

/// The portal can open the desktop's own shortcut settings (version 2)
pub fn can_configure(app: &AppHandle) -> bool {
  in_use(app) && app.state::<PortalHotkeys>().version.load(Ordering::Acquire) >= 2
}

async fn connect(app: &AppHandle) -> Option<GlobalShortcuts> {
  let portal = app.state::<PortalHotkeys>();
  if let Ok(app_id) = AppID::try_from(APP_ID) {
    // must come before any other portal call on the session bus connection
    if let Err(error) = ashpd::register_host_app(app_id).await {
      log::debug!("Could not register with the portal's host app registry: {error}");
    }
  }
  match GlobalShortcuts::new().await {
    Ok(proxy) if proxy.version() >= 1 => {
      log::info!(
        "Global hotkeys through the desktop portal (GlobalShortcuts v{})",
        proxy.version()
      );
      portal.version.store(proxy.version(), Ordering::Release);
      portal.available.store(true, Ordering::Release);
      listen(app.clone(), &proxy).await;
      Some(proxy)
    }
    Ok(_) => {
      log::info!("The desktop portal has no GlobalShortcuts; no global hotkeys on Wayland");
      None
    }
    Err(error) => {
      log::info!("No GlobalShortcuts portal ({error}); no global hotkeys on Wayland");
      None
    }
  }
}

/// Activations run the hotkey's action; changes made in the desktop's
/// settings update the shown triggers.
async fn listen(app: AppHandle, proxy: &GlobalShortcuts) {
  match proxy.receive_activated().await {
    Ok(mut activated) => {
      let app = app.clone();
      tauri::async_runtime::spawn(async move {
        while let Some(event) = activated.next().await {
          let state = app.state::<HotkeysState>();
          if state.is_suspended() {
            continue;
          }
          match command(event.shortcut_id()) {
            Some(command) => crate::hotkeys::run_action(&app, command),
            None => log::warn!("Unknown portal shortcut {}", event.shortcut_id()),
          }
        }
      });
    }
    Err(error) => log::warn!("Could not listen for portal shortcuts: {error}"),
  }
  match proxy.receive_shortcuts_changed().await {
    Ok(mut changed) => {
      tauri::async_runtime::spawn(async move {
        while let Some(event) = changed.next().await {
          remember(&app, event.shortcuts());
          crate::hotkeys::republish(&app);
        }
      });
    }
    Err(error) => log::warn!("Could not listen for portal shortcut changes: {error}"),
  }
}

fn remember(app: &AppHandle, shortcuts: &[Shortcut]) {
  let portal = app.state::<PortalHotkeys>();
  let mut assigned = portal.assigned.lock().unwrap();
  assigned.clear();
  for shortcut in shortcuts {
    assigned.insert(
      shortcut.id().to_string(),
      shortcut.trigger_description().to_string(),
    );
  }
}

/// The settings' view of the bindings: what the desktop assigned to each.
pub fn results(app: &AppHandle, bindings: &[Binding]) -> Vec<BindingResult> {
  let portal = app.state::<PortalHotkeys>();
  let assigned = portal.assigned.lock().unwrap();
  bindings
    .iter()
    .map(|binding| {
      let trigger = assigned.get(&id(binding.command)).cloned();
      let (ok, error) = match &trigger {
        Some(trigger) if !trigger.is_empty() => (true, None),
        Some(_) => (
          false,
          Some("No key assigned. Set one in your desktop's shortcut settings.".to_string()),
        ),
        None if xdg_trigger(&binding.key).is_none() => (
          false,
          Some("This key can't be used as a global shortcut".to_string()),
        ),
        None => (false, Some("Not accepted by the desktop".to_string())),
      };
      BindingResult::portal(binding, ok, error, trigger.filter(|t| !t.is_empty()))
    })
    .collect()
}

/// Binds @bindings through the portal. Returns false when there is no
/// GlobalShortcuts portal, so the caller reports hotkeys as unsupported.
pub async fn apply(app: &AppHandle, bindings: &[Binding]) -> bool {
  let portal = app.state::<PortalHotkeys>();
  let mut inner = portal.inner.lock().await;

  if !portal.probed.swap(true, Ordering::AcqRel) {
    if let Some(proxy) = connect(app).await {
      *inner = Some(Inner {
        proxy,
        session: None,
        bound: Vec::new(),
      });
    }
  }
  let Some(inner) = inner.as_mut() else {
    return false;
  };

  let wanted: Vec<(String, Option<String>)> = bindings
    .iter()
    .map(|binding| (id(binding.command), xdg_trigger(&binding.key)))
    .collect();
  if inner.session.is_some() && inner.bound == wanted {
    return true;
  }

  // a portal binds a session's shortcuts once, so changed keys take a new one
  if let Some(session) = inner.session.take() {
    if let Err(error) = session.close().await {
      log::debug!("Could not close the old portal shortcut session: {error}");
    }
  }
  portal.assigned.lock().unwrap().clear();

  let session = match inner
    .proxy
    .create_session(CreateSessionOptions::default())
    .await
  {
    Ok(session) => session,
    Err(error) => {
      log::warn!("Could not start a portal shortcut session: {error}");
      return true;
    }
  };
  let shortcuts: Vec<NewShortcut> = bindings
    .iter()
    .map(|binding| {
      NewShortcut::new(id(binding.command), description(binding.command))
        .preferred_trigger(xdg_trigger(&binding.key).as_deref())
    })
    .collect();

  match inner
    .proxy
    .bind_shortcuts(&session, &shortcuts, None, BindShortcutsOptions::default())
    .await
    .and_then(|request| request.response())
  {
    Ok(response) => {
      remember(app, response.shortcuts());
      log::info!(
        "Portal shortcuts bound: {:?}",
        response
          .shortcuts()
          .iter()
          .map(|s| format!("{}={}", s.id(), s.trigger_description()))
          .collect::<Vec<_>>()
      );
    }
    Err(error) => log::warn!("The desktop didn't bind the shortcuts: {error}"),
  }
  inner.session = Some(session);
  inner.bound = wanted;
  true
}

/// Opens the desktop's own settings for these shortcuts (portal version 2).
pub async fn configure(app: &AppHandle) -> Result<(), String> {
  let portal = app.state::<PortalHotkeys>();
  let inner = portal.inner.lock().await;
  let Some(inner) = inner.as_ref() else {
    return Err("No GlobalShortcuts portal".to_string());
  };
  let Some(session) = inner.session.as_ref() else {
    return Err("No shortcuts are bound yet".to_string());
  };
  inner
    .proxy
    .configure_shortcuts(session, None, ConfigureShortcutsOptions::default())
    .await
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
  use super::xdg_trigger;

  #[test]
  fn accelerators_become_xdg_triggers() {
    assert_eq!(
      xdg_trigger("CommandOrControl+Shift+H").as_deref(),
      Some("CTRL+SHIFT+h")
    );
    assert_eq!(xdg_trigger("Alt+F4").as_deref(), Some("ALT+F4"));
    assert_eq!(xdg_trigger("Super+Space").as_deref(), Some("LOGO+space"));
    assert_eq!(xdg_trigger("Ctrl+Digit1").as_deref(), Some("CTRL+1"));
    assert_eq!(xdg_trigger("Shift+ArrowUp").as_deref(), Some("SHIFT+Up"));
    assert_eq!(xdg_trigger("Ctrl+Shift").as_deref(), None);
    assert_eq!(xdg_trigger("Ctrl+NotAKey").as_deref(), None);
  }
}
