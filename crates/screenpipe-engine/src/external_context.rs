// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

//! Foreground app/title reported by a helper inside a VM, used to relabel the
//! opaque VM window on the host. Hot path (capture): a miss is one atomic load,
//! no allocation, no locks.

use arc_swap::ArcSwapOption;
use std::sync::Arc;
use std::time::{Duration, Instant};

const HOST_APP: &str = "Parallels Desktop";
const MAX_AGE: Duration = Duration::from_millis(5000);

pub struct ExternalContext {
    pub app_name: String,
    pub window_title: String,
    pub epoch: u64,
    pub seq: u64,
    pub received_at: Instant,
}

pub enum Accept {
    Stored,
    Cleared,
    /// (epoch, seq) not newer than the current one.
    Stale,
}

pub type ContextStore = ArcSwapOption<ExternalContext>;

pub static GLOBAL: ContextStore = ArcSwapOption::const_empty();

/// Held by tests that touch `GLOBAL`.
#[cfg(test)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Accepts iff (epoch, seq) is lexicographically newer than the stored one.
/// An empty `app_name` stores a tombstone that keeps its (epoch, seq).
pub fn accept(
    store: &ContextStore,
    epoch: u64,
    seq: u64,
    app_name: String,
    window_title: String,
    now: Instant,
) -> Accept {
    let new = Arc::new(ExternalContext {
        app_name,
        window_title,
        epoch,
        seq,
        received_at: now,
    });
    let mut stale = false;
    store.rcu(|cur| {
        stale = cur.as_ref().is_some_and(|c| (c.epoch, c.seq) >= (epoch, seq));
        if stale {
            cur.clone()
        } else {
            Some(new.clone())
        }
    });
    if stale {
        Accept::Stale
    } else if new.app_name.is_empty() {
        Accept::Cleared
    } else {
        Accept::Stored
    }
}

pub fn current_seq(store: &ContextStore) -> Option<(u64, u64)> {
    store.load().as_ref().map(|c| (c.epoch, c.seq))
}

/// Returns the VM app/title to use instead of the host app, only when the
/// screenshot was taken at exactly the stored (epoch, seq) and it is fresh.
pub fn override_for(
    store: &ContextStore,
    host_app: Option<&str>,
    monitor_hosts_focus: bool,
    seq_at_screenshot: Option<(u64, u64)>,
    now: Instant,
) -> Option<(String, String)> {
    let guard = store.load();
    let c = guard.as_ref()?;
    if !monitor_hosts_focus
        || c.app_name.is_empty()
        || !host_app.is_some_and(|a| a.eq_ignore_ascii_case(HOST_APP))
        || seq_at_screenshot != Some((c.epoch, c.seq))
        || now.saturating_duration_since(c.received_at) > MAX_AGE
    {
        return None;
    }
    Some((c.app_name.clone(), c.window_title.clone()))
}

/// Labels to store for a captured frame. The startup capture is excluded: it
/// runs before any focus event, so nothing ties the stored context to it.
pub fn guest_labels(
    store: &ContextStore,
    host_app: Option<&str>,
    monitor_hosts_focus: bool,
    is_startup: bool,
    seq_at_screenshot: Option<(u64, u64)>,
    now: Instant,
) -> Option<(String, String)> {
    // Cheap gates first so a miss costs no store load beyond the seq snapshot.
    if is_startup
        || !monitor_hosts_focus
        || !host_app.is_some_and(|a| a.eq_ignore_ascii_case(HOST_APP))
    {
        return None;
    }
    override_for(store, host_app, monitor_hosts_focus, seq_at_screenshot, now)
}

pub fn clear(store: &ContextStore) {
    // Tombstone, not None: a late post with an old (epoch, seq) must stay Stale.
    store.rcu(|cur| {
        cur.as_ref().map(|c| {
            Arc::new(ExternalContext {
                app_name: String::new(),
                window_title: String::new(),
                epoch: c.epoch,
                seq: c.seq,
                received_at: c.received_at,
            })
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const HOST: &str = "Parallels Desktop";

    fn put(s: &ContextStore, epoch: u64, seq: u64, app: &str, now: Instant) -> Accept {
        accept(s, epoch, seq, app.into(), "t".into(), now)
    }

    #[test]
    fn stores_and_overrides() {
        let s = ContextStore::empty();
        let now = Instant::now();
        assert!(matches!(put(&s, 1, 1, "MultiTerminal", now), Accept::Stored));
        assert_eq!(
            override_for(&s, Some(HOST), true, current_seq(&s), now),
            Some(("MultiTerminal".into(), "t".into()))
        );
    }

    #[test]
    fn wrong_host_app_is_none() {
        let s = ContextStore::empty();
        let now = Instant::now();
        put(&s, 1, 1, "MultiTerminal", now);
        assert_eq!(override_for(&s, Some("Safari"), true, current_seq(&s), now), None);
    }

    #[test]
    fn monitor_without_focus_is_none() {
        let s = ContextStore::empty();
        let now = Instant::now();
        put(&s, 1, 1, "MultiTerminal", now);
        assert_eq!(override_for(&s, Some(HOST), false, current_seq(&s), now), None);
    }

    #[test]
    fn expires_after_five_seconds() {
        let s = ContextStore::empty();
        let now = Instant::now();
        put(&s, 1, 1, "MultiTerminal", now);
        let seq = current_seq(&s);
        assert_eq!(
            override_for(&s, Some(HOST), true, seq, now + Duration::from_millis(5001)),
            None
        );
        assert!(override_for(&s, Some(HOST), true, seq, now + Duration::from_millis(4999)).is_some());
    }

    #[test]
    fn older_seq_is_rejected() {
        let s = ContextStore::empty();
        let now = Instant::now();
        put(&s, 1, 5, "Five", now);
        assert!(matches!(put(&s, 1, 4, "Four", now), Accept::Stale));
        assert_eq!(
            override_for(&s, Some(HOST), true, current_seq(&s), now).unwrap().0,
            "Five"
        );
    }

    #[test]
    fn new_epoch_replaces() {
        let s = ContextStore::empty();
        let now = Instant::now();
        put(&s, 1, 9, "A", now);
        assert!(matches!(put(&s, 2, 1, "B", now), Accept::Stored));
    }

    #[test]
    fn older_epoch_is_rejected() {
        let s = ContextStore::empty();
        let now = Instant::now();
        put(&s, 2, 1, "A", now);
        assert!(matches!(put(&s, 1, 99, "B", now), Accept::Stale));
    }

    #[test]
    fn empty_app_clears() {
        let s = ContextStore::empty();
        let now = Instant::now();
        put(&s, 1, 1, "A", now);
        assert!(matches!(put(&s, 1, 2, "", now), Accept::Cleared));
        assert_eq!(override_for(&s, Some(HOST), true, current_seq(&s), now), None);
    }

    #[test]
    fn post_after_clear_needs_newer_seq() {
        let s = ContextStore::empty();
        let now = Instant::now();
        assert!(matches!(put(&s, 1, 5, "", now), Accept::Cleared));
        assert!(matches!(put(&s, 1, 4, "A", now), Accept::Stale));
        assert!(matches!(put(&s, 1, 6, "A", now), Accept::Stored));
    }

    #[test]
    fn clear_keeps_tombstone_ordering() {
        let s = ContextStore::empty();
        let now = Instant::now();
        put(&s, 1, 5, "A", now);
        clear(&s);
        assert_eq!(override_for(&s, Some(HOST), true, current_seq(&s), now), None);
        assert!(matches!(put(&s, 1, 5, "A", now), Accept::Stale));
        assert!(matches!(put(&s, 1, 6, "A", now), Accept::Stored));
    }

    #[test]
    fn host_match_is_case_insensitive() {
        let s = ContextStore::empty();
        let now = Instant::now();
        put(&s, 1, 1, "A", now);
        assert!(override_for(&s, Some("parallels desktop"), true, current_seq(&s), now).is_some());
    }

    #[test]
    fn guest_labels_when_fresh() {
        let s = ContextStore::empty();
        let now = Instant::now();
        put(&s, 1, 1, "MultiTerminal", now);
        assert_eq!(
            guest_labels(&s, Some(HOST), true, false, current_seq(&s), now),
            Some(("MultiTerminal".into(), "t".into()))
        );
    }

    #[test]
    fn no_guest_labels_on_startup() {
        let s = ContextStore::empty();
        let now = Instant::now();
        put(&s, 1, 1, "MultiTerminal", now);
        assert_eq!(
            guest_labels(&s, Some(HOST), true, true, current_seq(&s), now),
            None
        );
    }

    #[test]
    fn no_guest_labels_for_other_host() {
        let s = ContextStore::empty();
        let now = Instant::now();
        put(&s, 1, 1, "MultiTerminal", now);
        assert_eq!(
            guest_labels(&s, Some("Safari"), true, false, current_seq(&s), now),
            None
        );
    }

    #[test]
    fn stale_seq_does_not_override() {
        let s = ContextStore::empty();
        let now = Instant::now();
        put(&s, 1, 2, "MultiTerminal", now);
        assert_eq!(
            guest_labels(&s, Some(HOST), true, false, Some((1, 1)), now),
            None
        );
    }
}
