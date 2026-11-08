# Design: "Your own devices trust each other" (a paired-devices group with auto-follow)

Target release: 0.1.13 (the user asked for everything in this release; "0.1.14" below means 0.1.13). Branch `agent-comms`.

## 0. Summary of the recommendation
- **A "Your devices" group is a third kind of container**, next to folders and channels. It is built like a channel with no messages: it has its own Ed25519 key pair, and that key is the **user key U**. It has a membership (`<id>.members`) and a `sync.toml` entry, and it uses the same discovery and sessions. Every paired device is an **Owner** of the group, so every device holds U.
- **Pairing** reuses the invitation machinery as it is. One device makes a one-use pairing link that expires in 15 minutes. The other device pastes it. The **issuing device asks the user to confirm** before admitting. On admission the new device is promoted to Owner and gets U through the existing `OwnerKey` message.
- **"On all my devices"** is a new signed statement in the group (kind 9, `Follow`). It lists the folders and channels that follow. A sibling joins them by itself. **Any sibling that is owner or admin of that folder or channel signs an ordinary kind-1 grant** for the other siblings. These grants are bytes that 0.1.12 and 0.1.13 already accept, so the cryptography never depends on user names.
- **Removal is final inside the group, and changes the key.** A removed device key can never be granted again in it, and the group moves on under a new user key U′ that the removed device never gets, so the U it keeps is worth nothing. Removal also cascades: the removing device signs Remove statements in each folder or channel where it is owner. If the device that does the removing is itself removed by a thief, the answer is **"Start Over"**, which makes a new group.

## 1. Identity and keys
- **Choice: a shared user key, carried as the group's owner key**, rather than device keys that cross-sign each other. Reasons:
  - It fits the existing statements, roles, `OwnerKey`, discovery (DHT topic and LAN tag from U's public key) and links with almost no new code.
  - The user asked that any of their devices can pair a new one. That means every device is equal anyway, so cross-signing would not reduce what a stolen device can do. Device keys that cross-sign each other would need a new trust-graph resolver and a new answer for two devices removing each other, for the same threat model.
- **Where U is kept:** in the same `SecretStore` as folder and channel secrets, under `folder-<group id>` (`crates/sync-net/src/roles.rs::secret_name`).
  - Mac: the data-protection keychain where the build has the entitlement, otherwise the 0600 file.
  - Windows and the daemon: the `sync/secrets` file in the state folder. Windows passes no vault today; protecting that file with DPAPI is a worthwhile hardening, listed under later work.
  - U is never written to `sync.toml`, never logged, and **never returned through the agent interface, not even at Full**. This is an explicit exception to the Full-permission rule for keys in SPEC §7G.1.
- **Pairing a device:** Settings › Devices › *Pair a Device…* on device A shows a link `seedwell://devices/<invite>?name=<A's name>&a=<sealed addresses>`. It is the existing `InviteLink` with a new `LinkKind::Devices`, role Admin, one use, 15 minutes. The issuer is A's device key, which records who paired whom.
  - The user copies it to the new device in any way, and Mac↔Windows works fine as text. A QR code comes later; desktops rarely have cameras.
  - The new device N pastes it under *Pair with a Link…*. N dials the sealed addresses as today.
  - A's `roles::check` sees a valid redeem for the group. Instead of admitting at once, it records a pending request and A shows: "“Studio PC” (MFZWI3D-…) wants to join your devices. **Pair** / Refuse". N shows its own id, so the user can compare the two screens.
  - After *Pair*, A signs an owner grant `Grant{N, Owner, invite}` and sends U through `OwnerKey`.
  - This link-plus-confirmation is real two-factor trust: whoever holds the link also needs someone at A's screen. A leaked link (for example pasted into a channel) is useless without that confirmation.
- **Removing a device (for example a lost laptop):** *Remove…* on any sibling signs `Remove{L}` with U in the group. New rule for this container kind only: **a Remove in a devices group is final**, so later grants for L are ignored. This stops L, which still holds U, from granting itself back in. What removal revokes:
  1. L's group sessions close, and its later group statements (follows, profiles) are ignored.
  2. No sibling auto-admits L anywhere again.
  3. Each sibling that is **Owner** of a folder or channel where L is a member signs the existing kind-2 Remove there. The dialog lists these places, all ticked. Places where siblings are only admins are listed as "ask its owner".
  4. Devices L paired (the invite issuer is L) are flagged in the list as "paired from L". The user keeps or removes each one.

  5. **The key changes** (see *Key change after a removal* below), so the U that L keeps is worth nothing: a device L signs into the old group with it finds nobody who admits it.

  What removal cannot revoke: the files and messages that L already has. That is the existing rule for any removed member.
- **Key change after a removal** (implemented in v1; added after review, because without it a removed device that keeps U can mint fresh devices that every sibling auto-admits):
  - The removing device makes a new group with a new key U′: owner grants (signed with U′) for every current member except the removed ones, the folders and channels on all devices (Follow statements signed again by its device key), and the devices it knows (names, addresses). It signs a **Moved** statement (kind 10, `target pk ‖ at_s`, by its device key) in the old group, sends the old group's statements to the siblings connected now, and deletes U.
  - A sibling that sees a current member's Moved in its group moves: it signs the same Moved there (so others that meet only it follow too), deletes U, keeps the old group's statements without the key (the `previous` list of `[my_devices]`, at most 8), and joins the new group with an **empty proof**: the new group's members know its device key from the carried-over grants and admit it at once; the existing OwnerKey transfer then gives it U′ over that authenticated session.
  - A sibling away at the time is still in the old group. When it next meets any sibling of the new group (over any session: a shared folder, the address it last had), it names the old group in its DevicesJoin; that sibling recognises its device key in the new group's list and answers with its grant there and the Moved statements (as proofs), so the away device moves too. Groups a device moved on from are never announced, looked up or answered for anyone else: a device minted with the old key finds no one.
  - **Two removals at once** (each made apart, so two new groups): every device takes, of the Moved statements of current members, the **lowest new group id**; once two devices that split meet, they exchange what each knows of the old group (only between devices that are in each other's current group), the one in the losing group moves to the winner (saying so in the loser, so its members follow), and once it holds the winner's key it removes there the devices it had removed in the old group, which changes the key again. Moves from a group a device moved on from count only when made by devices of its current group, so a device minted with an old key cannot redirect anyone who has moved.
  - **Leave** signs this device's own removal; the siblings that see it change the key themselves (several at once settle as above). They do so over sessions that are up, so a sibling often hears of another's Moved while it is switching to its own new group: the old group it keeps (`previous`) is the same object that took that statement in, never read again from its file at the switch, so the move is not lost and it then moves to the winner (`open_devices`; tests `two_devices_changing_the_key_at_once_settle_on_one_group` and the three-device one). Until 0.1.19 it re-read the old group from its file at the switch, and a Moved that came in after that read was lost: the two siblings stayed in two groups for good.
  - What stays: a sibling that has not moved yet still trusts the old group's members, so a thief who signs a Moved there before the honest removal reaches it can lead it astray; and a thief who removes the honest devices first still leaves only *Start Over*. Devices "paired by" records do not carry over (a grant's invitation names the old group).
- **Start Over** (Settings › Devices): leave the group, deleting all of it on this device, and make a new one. The user then pairs the other devices again. This is the way out when a thief removed the honest devices first, or when U must be considered stolen.
- **The rendezvous is shared** (added with the rendezvous of SPEC §7G.3): a device that uses one signs a **rendezvous** statement in the group (kind 11, `url ‖ serial ‖ at_s`, its device key; its latest counts; empty = none). A sibling that never said one takes the newest non-empty one (`adopt_rendezvous_from_paired`) and then says it too; turning it off on one device signs "none" there and nothing else changes. What a device said last is also looked up in the groups it moved on from, so a key change keeps the choice. The pairing link carries the maker's rendezvous in its `a=` block (`own_reach`). Siblings are looked up in the rendezvous like any wanted device (the dial loop goes over the group too), and a network change redials the group's sessions like the others.

## 2. How it fits "records stay in their container"
- The group **is** a container: `SpaceKind::Devices` in `crates/sync-net/src/shared.rs`. It holds:
  - `sync/<group id>.members`: grants, removes, the profile statements with device and user names, and the Follow statements.
  - A `[my_devices]` table in `sync.toml`: id, `group_pk`, `devices: Vec<SpaceDevice>` (addresses, last seen), `skip` (this device's opt-outs) and the follow mode. **Do not use the key `device`**: that is the read-only list from 0.1.8 (`SyncConfig::devices`).
  - Secrets: `folder-<id>` (U), plus `invite-<id>` and `links-<id>` while pairing.
- Leaving or starting over deletes exactly these. Folders and channels stay, because they are their own containers. Which folders are "followed" is never stored in a folder's entry; it comes only from the group's Follow statements.
- Sessions: a new message type 28, `DevicesJoin`, with the same records as `ChatJoin` (id ‖ proofs), behind **feature bit 4 `devices`**. Members (type 3) is reused unchanged. A device proves membership of the group only to devices it believes are siblings, so a stranger learns nothing.
- Discovery uses U's public key (DHT topic, LAN tag), as for any container.
- Every caller of `registry.spaces()` must be checked for the new kind. `relay::may_relay` should later count siblings as "own devices"; today it guesses from "both may write a shared folder".
- For agents the group is treated like a No channel: it is never listed, and `sync.list` does not list a device this Mac is connected to only for the group.

## 3. Auto-follow
- **Statement kind 9 `Follow`** (`members.rs`), signed by a sibling's device key so the announcing device is known:
  - Body: `target u8 (1 folder, 2 channel) ‖ target pk (32) ‖ name str ‖ on u8 ‖ serial varint ‖ at_s varint [‖ later fields: the pk of a folder's chat]`.
  - For each target, the latest statement counts, ordered by serial, then time, then signature. It is accepted only from a current member and only inside a devices group; elsewhere it means nothing.
- **What follows:**
  - Folders the user marks *On all my devices*.
  - Channels the user marks the same way. This includes Agents: No channels. They follow, and on every device they stay invisible to agents, because their own kind-8 statement decides.
  - Nothing follows on its own: an agent's `chat.new` or `sync.add` never creates a Follow.
  - The toggle is offered only where this device is **Owner or Admin**. Otherwise no sibling can admit the others, and the line says so.
  - Defaults: on in the user's New Channel window (only when paired); off in Add Folder.
- **On each sibling, when it learns of a Follow with `on`** for something it does not have and that is not in its `skip` list:
  - A folder is placed at the **default location for joined folders**, named `<name>`, or `<name> 2` if taken (also when an on-demand folder's place has that name). A followed folder is **never merged silently** into an existing unsynced folder. It is synced in full in v1 (0.1.13).
  - **On demand (0.1.14).** The Follow carries flags after `at_s` (written only when not 0, so 0.1.13 reads the same Follow with bytes after it, keeps them, and its signature checks). Flag **on demand** (bit 0) is set when the folder is on demand on the device that turns *On all my devices* on. A sibling then joins it on demand: its place is `<default location>/<name>` (not made; the File Provider shows it), its files go to this device's cache, nothing is downloaded until asked for. A device that keeps no folders on demand (`seedwell_platform::ON_DEMAND_FOLDERS`: Linux today) joins it in full and its page shows the issue "On demand on your other device, but this PC can't keep folders on demand yet: every file is downloaded here." The flag is only the default at joining: switching the folder later on one device changes no other device, and a sibling that joined in full under 0.1.13 stays full.
  - A channel gets a `ChannelEntry`.
  - Either one starts in a new state, "following, not admitted". It is a joining space whose `invite` is `None`. `Space::joining` and `roles::proofs` learn this state, and its Join proof is the new **proof kind 6 `Sibling`**, whose bytes are the group id. The peer's identity is already proved by its TLS device key, so no exporter binding is needed.
- **Admission, two complementary paths. Both produce ordinary statements:**
  - **Push.** When a device becomes a sibling, or a Follow is turned on, every sibling that is owner or admin of the target signs grants for every sibling that is not a member there yet, and not removed there by an owner:
    - an owner signs `Grant{D, Admin, invite: None}` with the folder key;
    - an admin creates a one-use invite with itself as issuer, role Read & write, expiring in 1 hour, and signs `Grant{D, ReadWrite, invite}`, which is exactly an admin admission today.

    The grants spread through the normal Members exchange. So any member, including other people's devices and devices on 0.1.12 or 0.1.13, accepts the sibling's session: `roles::check` returns Member from `role_of(peer)` without any proof it understands.
  - **Pull.** When N proves `Sibling` to an owner or admin sibling that has not signed yet, that sibling signs on the spot, along the same code path.
- **Roles:**
  - The default role is **Admin**, not co-owner. A co-owner holds the folder secret `S` and can never really be removed, while an admin can.
  - An owner sibling raises a sibling it finds at Read & write to Admin. It never lowers a role.
  - **Whose Follow counts** (0.1.19): for a folder or channel a device has, the latest Follow of a device that is owner or admin of it there (`Membership::follows_by`); a member's alone (a sibling made Read only, another build) turns nothing off, removes nothing (only an owner's removed Follow removes a folder, and so its key) and lifts nothing. A device that waits to be admitted, or does not have it, goes by the plain latest; an owner or admin restates its statement when a member's differs. Serials count up to `MAX_FOLLOW_SERIAL` (2^32), next ones saturate and are numbered above the owners' and admins' only; a new Follow is always a change (saved and sent on). A rotation carries each Follow as it counts, at its own time.
  - **Read only** (0.1.20) is no longer a Follow's matter: it is the folder's own setting, a statement only its owners sign (kind 12, SPEC §7G.1 *Read only*). While it is on, only the owners' changes count: every other member, the user's other devices included, keeps its role and holds its changes back (`roles::held_read_only`), and no device takes a version a member made. 0.1.19's Follow flag `FOLLOW_READ_ONLY` (4), which lowered the user's other devices to Read only, is no longer written. Once per folder (`read_only_migrated`) an owner gives each sibling that design lowered the role it had before (Admin for one first admitted at Read only), and when that Read only was on (`devices_read_only`, then deleted, or the flag on the group's Follows), turns the folder's own Read only on in its place (`devices::migrate_read_only`).
  - Making a second device co-owner stays a deliberate step in the existing role menu. The Devices page explains why that is worth doing as a backup.
- **Opting out and turning off:**
  - *Not on This Mac* on a follower adds the item to `skip`. When removing a followed folder or channel, the dialog asks: "Only from this Mac (it won't come back)" or "Stop putting it on all my devices". (Not built: removing on a device that is not the folder's owner is "only from this device"; on its owner, see below.)
  - Turning *On all my devices* off signs `Follow{on: 0}`. Siblings keep what they have, and nothing is deleted anywhere. New siblings just stop getting it.
  - **The owner removes it (0.1.14).** Removing a followed folder in the app on the device that is its **Owner** signs `Follow{on: 0, flags: removed (bit 1)}` once the removal here succeeded. Every sibling that sees it removes the folder from Seedwell with the ordinary removal: a full folder's files stay where they are, an on-demand folder's cache becomes an ordinary folder at its place; nothing goes to the Trash. A sibling away at the time does it when the group's statements reach it. Each device notes the removal (`removed` in `[my_devices]`), so a folder joined again afterwards stays; the note goes when the folder follows again. A key change carries removed Follows that some sibling may not have acted on yet. The Remove question on the owner adds: "It is also removed from Seedwell on your other devices; their files stay where they are." (`SyncFolder.removes_everywhere`, `sync_remove_folder_question`.) A removal on a non-owner sibling, and any removal through the agent interface (`sync.remove`, so the CLI too), stays on that device (the skip list). Channels are left as they are: leaving one never removes it on the other devices.

## 4. Security
- **Matching user names never grants anything.** Names come from profile statements and are shown only.
- **A thief with one unlocked device running Seedwell** gets:
  - everything on it: files, messages, and the keys in its store, including U;
  - the ability to pair more devices (confirming on its own screen). Those devices are auto-admitted as Admin wherever siblings may admit. Every sibling shows "“X” was paired by “L”" on the Devices page. A system notification for this comes later.

  The user removes L and anything L paired: the removals are final and change the key, so the U that L keeps (and any device it signs in with it afterwards) is worth nothing. The thief can also remove honest devices; then the user does *Start Over*. A thief can never make a sibling delete files: unfollowing deletes nothing, and removal never deletes.
- **A thief with only the disk:** on a Mac with FileVault and the keychain, nothing. On Windows the plaintext secrets file is a risk, hence the DPAPI item.
- **Agents, at every level including Full:** no method pairs, confirms, removes, follows, leaves, starts over or reads U. Only the app's UI calls these FFI functions, as with the private-message switch. Agents see followed Yes channels and folders like any other, and nothing else of the group. The CLI gets no pairing commands, because the CLI uses the agent interface.
- **Private (Agents: No) channels** follow but stay hidden on every device. While a channel is "following, not admitted", it is closed to agents (there is no `agents=yes` hint).

## 5. Compatibility with 0.1.12 and 0.1.13
- **Older devices never see the group:**
  - type 28 and proof 6 are behind feature bit 4 (otherwise they are an unknown type or proof: "carried, never trusted");
  - `seedwell://devices/` links are refused as not a Seedwell link;
  - kind 9 only ever lives in a group membership.
- **In folders and channels**, older devices receive only kind-1 grants (with or without an invite) and kind-2 removes. They apply these, pass them on, and accept the sibling's sessions. A one-use invite created by an admin is used up at once, so no older Share dialog lists it.
- **Pairing needs both devices on 0.1.14.** The joining device's sentence for an older issuer is already there: "runs an older Seedwell: update it".
- **Downgrading a paired device:** the older `sync.toml` writer drops `[my_devices]`, so that device forgets the group (re-pair after upgrading). Its secrets are left orphaned in the store. Its folders keep syncing.
- **Golden vectors gain new entries:** kind 9, proof 6, type 28 and the devices link. Existing bytes do not change.

## 6. UI (Rust builds the texts in a new `crates/ffi/src/devices_text.rs`; the hosts only draw)
- **Settings › Devices** (a new tab on the Mac; a section of `SettingsPage.cs` on Windows):
  - Intro line: "Your devices trust each other: folders and channels you put on all your devices come to each of them by themselves."
  - The list, this device first: name, user, online or last seen, "paired by …", and *Remove…* on the others.
  - Buttons *Pair a Device…* (link with Copy, countdown, then the confirmation with the device id), *Pair with a Link…*, and *Leave Your Devices…* or *Start Over…*.
  - The remove sheet lists the folders and channels it is also removed from, with ticks.
- **The "On all my devices" toggle:**
  - In the folder Add and Edit sheets (Mac: `Sync/SyncModals.swift`; Windows: `FolderSyncPage.cs`), with the line "Your other devices (MacBook, Studio PC) join it by themselves." or the reason it is disabled. Below it, but not part of it, the folder's own *Read only* switch (`ReadOnlyLine`, shown for every folder whether or not this device is paired; only an owner of the folder can switch it; also in the Share dialog, applied at once there).
  - In the channel ⋯ menu as *On All My Devices* with a checkmark, and in the New Channel window (Mac: `Chat/ChatActions.swift`, `Window/ChatViews.swift`; Windows: the channel context menu in `LibraryPane.cs`).
- **Joining sentences** (`crates/sync-net/src/joining.rs`): "Waiting for one of your devices that can admit this Mac to “Dev” (Mac mini)…". The folder or channel shows "from your devices".

## 7. Scope cut
**v1 is the smallest set that makes a new Mac appear in "Seedwell Development" and the dev folder by itself:**
- the group container;
- the pairing link with confirmation on the issuer;
- the device list, final removal with the owner cascade and the key change, Leave/Start Over;
- the Follow statement and the two toggles;
- push and pull admission (Admin from an owner, Read & write from an admin);
- auto-join in full mode at the default location (0.1.14: on demand when the folder is on demand where it was put on all devices);
- the `skip` list;
- the agent exclusions.

**Later:**
- pairing over an existing session with a 6-digit code compared on both screens (no link at all, ideal between Mac and Windows);
- a QR code;
- an on-demand default per device (0.1.14 takes the mode of the device that put the folder on all devices instead);
- system notifications for new pairings;
- `may_relay` counting siblings;
- DPAPI on Windows;
- an optional "my devices are co-owners";
- a headless or daemon pairing flow;
- an agent-readable `all_my_devices` flag.

## 8. Implementation plan, in order
1. **sync-proto** (`members.rs`):
   - `Body::Follow` (kind 9: read, write, verify for device signers).
   - `Membership::new_devices(pk)` with final removals and `follows()`, giving the latest per target.
   - `LinkKind::Devices` with the prefix `seedwell://devices/`; the folder and chat parsers refuse it, and its parser refuses theirs.
   - `keys.rs`/`msg.rs`: `Proof::Sibling` (kind 6), `FEATURE_DEVICES` (bit 4), `DevicesJoin` (type 28, reusing `JoinChannel` records).
   - **Tests:**
     - round trips;
     - a kind-9 statement read as Unknown by an older-style reader and passed on byte for byte;
     - a removed device's later owner grant is ignored in a group but not in a folder;
     - a follow from a removed member is ignored;
     - serial ordering;
     - link parsing across kinds;
     - new golden vectors in `tests/it/golden.rs`;
     - updates to `PROTOCOL.md` (statement table, proof table, message table, feature table, link section).
2. **sync-net config and space:**
   - `SpaceKind::Devices` in `shared.rs`; check every `registry.spaces()` caller and `roles::noun`.
   - `[my_devices]` in `config.rs`.
   - A new `devices.rs`: `devices_pair_link`, `devices_join`, `devices_confirm`/`devices_refuse`, `devices_list`, `devices_remove` (with the cascade), `devices_leave`, `devices_start_over`, `set_follow`/`skip`, open at start, and a hook on group changes.
3. **Handshake** (`peer.rs`, `roles.rs`):
   - Send and check `DevicesJoin` with siblings only.
   - `roles::proofs`: return `Sibling` in the following state.
   - `roles::check`: sibling admission (Admin when this device is owner; a self-made one-use invite when it is admin), and a pending confirmation for a redeemed group invite.
   - Promotion to Owner plus `OwnerKey` after confirmation.
4. **Push and auto-join:**
   - On a group or Follow change, sign the missing sibling grants in each followed space; raise siblings from Read & write to Admin.
   - Create the "following" folder or channel entries (path rules from `join_folder`, never merging).
   - The joining sentences in `joining.rs`.
   - Removal prompts in `service.rs::remove_folder` and `chat_leave`.
5. **Control** (`crates/control`): no new methods. Hide the group and devices connected only for it from `sync.list`, `chat.list` and events. Test that U never appears in any key export.
6. **FFI and hosts:** records and calls in `crates/ffi/src/sync.rs`, texts in `devices_text.rs`, then the Mac Settings tab, folder sheet and channel menu, and the Windows Settings section, folder page and context menu.
7. **Integration tests** (a new `crates/sync-net/tests/it/devices.rs`, three or four services on loopback):
   1. Pairing A↔B: refused without confirmation, then Owner plus U after it. A device with the same user name but no link gets nothing.
   2. A follows folder F and a Yes channel C: B joins both by itself as Admin; files and messages arrive; a followed No channel is invisible to B's agent API.
   3. N is paired through B, which is only admin in F: N is admitted as Read & write, then raised to Admin when A comes online.
   4. A member running an older protocol range (the `versions.rs` style, without bit 4) accepts N's session from A's pushed grant.
   5. Removing N on A: final in the group, N cut from F and C, `Sibling` refused afterwards.
   6. `skip`: something removed from this Mac only does not come back. Unfollowing deletes nothing.
   7. Mutual removal, then Start Over.
   8. Key change: a removed device that keeps the old U mints a device, which gets nothing and is admitted by nobody; a sibling away during the removal moves to the new group when it comes back; two removals made apart settle on one group without either removed device.
   9. Update `tools/sync-compat.sh` to check that a 0.1.13 member syncs with an auto-admitted sibling (in v1: the CLI has no pairing command, so the script checks that; the sibling case runs in-process in test 4).
8. **SPEC:** a new §7G.1 subsection *Your devices*, §7G.5 Settings › Devices, and §9A, which says agents cannot pair or follow.

### Critical Files for Implementation
- /Volumes/EksternalHome/Koding/Rust/SeedWell/crates/sync-proto/src/members.rs
- /Volumes/EksternalHome/Koding/Rust/SeedWell/crates/sync-net/src/roles.rs
- /Volumes/EksternalHome/Koding/Rust/SeedWell/crates/sync-net/src/peer.rs
- /Volumes/EksternalHome/Koding/Rust/SeedWell/crates/sync-net/src/shared.rs (plus a new `crates/sync-net/src/devices.rs`)
- /Volumes/EksternalHome/Koding/Rust/SeedWell/crates/sync-net/src/config.rs
- /Volumes/EksternalHome/Koding/Rust/SeedWell/crates/sync-proto/PROTOCOL.md
