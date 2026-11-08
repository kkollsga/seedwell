//! A folder's "Read only" switch (SPEC §7G.1, §7G.5) as every host draws it: in the Add and Edit sheets and in the
//! Share dialog, for every folder of this computer, on its own (not under "On all my devices", so an owner whose
//! computer is paired with nothing has it too). Only an owner of the folder may switch it; everywhere else it shows the
//! state and who can switch it.

use seedwell_platform::computer;

use crate::backend::Backend;
use crate::types::CoreError;
use crate::Core;

/// "Read only": the folder's owners' setting. While on, only the owners' changes count: everyone else it is shared
/// with, the user's other devices too, opens and downloads its files but changes nothing in it, here or on each other.
/// Nobody's role changes.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ReadOnlyLine {
    /// "Read only".
    pub title: String,
    /// As the core has it; in a dialog's model, the form's choice.
    pub on: bool,
    /// It can be switched: this computer is an owner of the folder.
    pub enabled: bool,
    /// What it means as it is set, and what changes when it is applied.
    pub line: String,
    /// Its tooltip.
    pub help: String,
    /// This computer is an owner of the folder (the only one that may switch it).
    pub owner: bool,
}

/// The switch's tooltip.
const HELP: &str = "Only the owner's changes count: everyone else can open and download its files but change nothing in it";

/// The switch as the core has it: `on`, and whether this computer is an owner of the folder (`owner`: the only one that
/// may switch it).
pub(crate) fn read_only_line(on: bool, owner: bool) -> ReadOnlyLine {
    let r = ReadOnlyLine { title: "Read only".into(), on, enabled: owner, line: String::new(), help: HELP.into(), owner };
    ReadOnlyLine { line: read_only_drawn(&r, on).line, ..r }
}

/// The switch drawn for a dialog: `core` as the core has it and `chosen` the form's choice (as it is when this
/// computer may not switch it). Its line says what it means and what Save (or the switch, where it applies at once)
/// changes.
pub(crate) fn read_only_drawn(core: &ReadOnlyLine, chosen: bool) -> ReadOnlyLine {
    let chosen = if core.enabled { chosen } else { core.on };
    let what = "can open and download its files but change nothing in it, here or on each other";
    let line = if !core.enabled {
        if core.on {
            format!("Read only: only the folder's owner changes it. Changes made on this {} stay here until the owner turns it off.", computer!())
        } else {
            "Only the folder's owner can make it read only.".to_string()
        }
    } else {
        match (core.on, chosen) {
            (_, true) => format!("Only this {}'s changes count: everyone it is shared with, your other devices too, {what}. Nobody's role changes.", computer!()),
            (true, false) => format!(
                "Everyone can change it again as their role allows; what they changed meanwhile comes in (a conflict where this {} changed the same file).",
                computer!()
            ),
            (false, false) => "Off: everyone it is shared with can change it as their role allows.".to_string(),
        }
    };
    ReadOnlyLine { on: chosen, line, ..core.clone() }
}

/// What Save or Start changes: `Some(chosen)` when this computer may switch it and the choice differs from `core`.
pub(crate) fn read_only_change(core: Option<&ReadOnlyLine>, chosen: bool) -> Option<bool> {
    core.filter(|r| r.enabled && r.on != chosen).map(|_| chosen)
}

/// Folder `id`'s switch as `backend` has it (`None`: not a folder synced here, or a channel).
pub(crate) fn read_only_line_for(backend: &dyn Backend, id: &str) -> Option<ReadOnlyLine> {
    backend.sync().read_only_state(id).map(|s| read_only_line(s.on, s.owner))
}

/// The app's own calls (agents switch it with `sync.set_read_only`).
impl Core {
    /// Turns folder `id`'s "Read only" on or off: its owners' setting (only an owner may; anyone else is refused, with
    /// the reason). While on only the owners' changes count; nobody's role changes.
    pub fn sync_set_read_only(&self, id: String, on: bool) -> Result<(), CoreError> {
        self.backend.sync().set_read_only(&id, on)
    }
}

#[uniffi::export]
impl Core {
    /// Folder `id`'s "Read only" switch as it is (`None`: not a folder synced here). Shown for every folder of this
    /// computer, switchable only on an owner.
    pub fn sync_read_only_line(&self, id: String) -> Option<ReadOnlyLine> {
        read_only_line_for(self.backend.as_ref(), &id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The switch is the owner's folder setting: it must offer the switch to an owner only (anyone else sees the state
    /// and is told who can switch it), say what applying it does (only the owner's changes count, nobody's role
    /// changes) and what turning it off brings (what others changed comes in), and Save must change it only on an owner.
    #[test]
    fn the_read_only_switch_is_the_owners_and_says_what_it_changes() {
        let owner = read_only_line(false, true);
        assert!(!owner.on && owner.enabled && owner.owner && owner.title == "Read only" && owner.help.contains("change nothing"), "{owner:?}");
        let on = read_only_drawn(&owner, true);
        assert!(on.on && on.line.contains("changes count") && on.line.contains("Nobody's role changes"), "{}", on.line);
        assert_eq!(read_only_change(Some(&owner), true), Some(true));
        let off = read_only_drawn(&read_only_line(true, true), false);
        assert!(!off.on && off.line.contains("comes in"), "{}", off.line);
        let member = read_only_line(true, false);
        assert!(member.on && !member.enabled && member.line.contains("only the folder's owner"), "{member:?}");
        assert!(read_only_drawn(&member, false).on, "a member cannot switch it off");
        assert_eq!(read_only_change(Some(&member), false), None, "Save switched it on a member");
        let member_off = read_only_line(false, false);
        assert!(member_off.line.contains("Only the folder's owner can make it read only"), "{member_off:?}");
        assert_eq!(read_only_change(None, true), None);
    }
}
