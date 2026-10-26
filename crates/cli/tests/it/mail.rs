//! `seedwell mail` between two daemons on loopback (SPEC §7G.8): a mail into another project's inbox, open until
//! its project closes it, the answer and the status back to the sender, `watch`, `check` and the hook.

use crate::common;

use std::io::{BufRead, BufReader, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};

use common::*;
use serde_json::Value;

/// `seedwell --json <args>` against `d`, in `dir` (default: the daemon's folder), with `input` on standard input.
fn run(d: &Daemon, dir: Option<&Path>, args: &[&str], input: &str) -> Out {
    let mut child = Command::new(BIN)
        .arg("--socket")
        .arg(&d.socket)
        .arg("--json")
        .args(args)
        .env_remove("SEEDWELL_SOCKET")
        .env_remove("SEEDWELL_AGENT")
        .env_remove("SEEDWELL_MAIL_CHANNEL")
        .current_dir(dir.unwrap_or_else(|| d.state.parent().unwrap()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    Out { code: o.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&o.stdout).into_owned(), stderr: String::from_utf8_lossy(&o.stderr).into_owned() }
}

fn json(d: &Daemon, args: &[&str], input: &str) -> Value {
    let o = run(d, None, args, input);
    assert_eq!(o.code, 0, "seedwell {args:?}: {}", o.stderr);
    serde_json::from_str(o.stdout.trim()).unwrap_or_else(|e| panic!("seedwell {args:?}: not JSON ({e}): {}", o.stdout))
}

#[test]
fn mail_goes_to_a_projects_inbox_and_its_status_comes_back() {
    let root = tempfile::tempdir().unwrap();
    let a = Daemon::start(root.path(), "a", "full");
    let b = Daemon::start(root.path(), "b", "full");
    a.json(&["chat", "new", "Koding"]);
    let link = a.json(&["chat", "share", "Koding"])["link"].as_str().unwrap().to_owned();
    assert_eq!(run(&b, None, &["chat", "join", "-"], &link).code, 0);
    let port = a.json(&["sync", "list"])["port"].as_u64().unwrap();
    b.json(&["chat", "address", "Koding", &format!("127.0.0.1:{port}")]);
    wait_until(30, "B admitted", || b.json(&["chat", "list"])["channels"][0]["role"].as_str().map(str::to_owned));
    // No channel open to agents yet: told where to switch it on.
    let o = run(&a, None, &["mail", "inbox", "--as", "api"], "");
    assert_eq!(o.code, 4, "{}", o.stderr);
    assert!(o.stderr.contains("Agents on This Mac"), "{}", o.stderr);
    for d in [&a, &b] {
        d.json(&["chat", "agents", "Koding", "post"]);
    }

    // web (on B) asks api (on A) for a field. The channel is found by itself (the one open to agents).
    let sent = json(&b, &["mail", "send", "api", "--as", "web", "--type", "request", "-"], "## Add eta_minutes\n\nThe order page shows an ETA.");
    let id = sent["id"].as_str().unwrap().to_owned();
    assert_eq!((id.len(), sent["to"].as_str(), sent["channel"].as_str()), (8, Some("api"), Some("Koding")), "{sent}");
    assert_eq!(run(&b, None, &["mail", "send", "web", "--as", "web", "-"], "x").code, 2, "not to its own inbox");

    let inbox = wait_until(30, "the mail at A", || {
        let v = json(&a, &["mail", "inbox", "--as", "api"], "");
        (v["open"] == 1).then_some(v)
    });
    let item = &inbox["items"][0];
    assert_eq!(
        (item["id"].as_str(), item["type"].as_str(), item["summary"].as_str(), item["state"].as_str(), item["new"].as_bool(), item["trust"].as_str()),
        (Some(id.as_str()), Some("request"), Some("Add eta_minutes"), Some("open"), Some(true), Some("untrusted-peer")),
        "{inbox}"
    );
    assert!(item["from"].as_str().unwrap().starts_with("web (agent) on "), "{item}");
    assert!(item.get("text").is_none(), "an inbox lists, it does not load every text: {item}");
    assert!(json(&a, &["mail", "inbox", "--as", "api"], "")["items"][0].get("new").is_none(), "shown once as new");
    let read = json(&a, &["mail", "read", &id, "--as", "api"], "");
    assert_eq!(read["text"].as_str(), Some("## Add eta_minutes\n\nThe order page shows an ETA."), "the text without its header: {read}");
    assert_eq!(run(&b, None, &["mail", "done", &id, "--as", "web", "-"], "x").code, 2, "only api closes api's items");

    // Tags (§7G.9): showing it tagged it seen for api; api adds backlog, and web sees both under api's name.
    let t = json(&a, &["mail", "tag", &id, "backlog", "--as", "api"], "");
    assert_eq!(t["yours"], serde_json::json!(["seen", "backlog"]), "{t}");
    assert_eq!(json(&a, &["mail", "inbox", "--as", "api", "--tag", "backlog"], "")["items"][0]["id"].as_str(), Some(id.as_str()));
    assert!(json(&a, &["mail", "inbox", "--as", "api", "--tag", "actioned"], "")["items"].as_array().unwrap().is_empty());
    let at_b = wait_until(30, "api's tags at B", || {
        let v = json(&b, &["chat", "tag", "Koding", &id, "--as", "web"], "");
        (v["tags"][0]["tags"] == serde_json::json!(["seen", "backlog"])).then_some(v)
    });
    assert_eq!((at_b["tags"][0]["owner"].as_str(), at_b["tags"][0]["agent"].as_bool()), (Some("api"), Some(true)), "{at_b}");
    assert!(at_b["yours"].as_array().unwrap().is_empty(), "web has none of its own: {at_b}");
    let t = json(&a, &["mail", "tag", &id, "--remove", "backlog", "--as", "api"], "");
    assert_eq!(t["yours"], serde_json::json!(["seen"]), "{t}");
    assert_eq!(run(&a, None, &["mail", "tag", &id, "not,a,tag", "--as", "api"], "").code, 2);
    // A tag note is no news: it neither opens an item nor shows in an agent's chat read.
    let lean = run(&b, None, &["chat", "read", "Koding", "--lean", "--as", "web"], "");
    assert!(!lean.stdout.contains("\"type\":\"tag\"") && !lean.stdout.contains("type: tag"), "{}", lean.stdout);

    // web waits; api answers and closes the item in one go.
    let mut watch = Command::new(BIN)
        .arg("--socket")
        .arg(&b.socket)
        .args(["--json", "mail", "watch", "--as", "web", "--timeout", "60"])
        .env_remove("SEEDWELL_MAIL_CHANNEL")
        .current_dir(b.state.parent().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    let r = json(&a, &["mail", "reply", &id, "--as", "api", "--done", "Added eta_minutes (v2); plan in docs/plans/orders.md", "-"], "Added: `eta_minutes`, an integer or null.");
    assert_eq!((r["to"].as_str(), r["re"].as_str(), r["status"].is_string()), (Some("web"), Some(id.as_str()), true), "{r}");
    // It prints all that is new (the answer, and the status when it came at once), then exits.
    let lines: Vec<String> = BufReader::new(watch.stdout.take().unwrap()).lines().map_while(Result::ok).collect();
    let status = watch.wait().unwrap();
    assert!(status.success(), "watch ended: {status}");
    // The watch settled long enough to get the reply and its status together: one final answer, said once.
    assert_eq!(lines.len(), 1, "{lines:?}");
    let got: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!((got["re"].as_str(), got["news"].as_str(), got["type"].as_str()), (Some(id.as_str()), Some("answer"), Some("reply")), "{got}");
    assert!(got["line"].as_str().unwrap().starts_with("api answered and closed your request"), "{got}");

    // The one `reply --done` closes it on both sides: done at A, and at B the final answer is no item to close.
    assert_eq!((r["final"].as_bool()), Some(true), "{r}");
    let all = json(&a, &["mail", "inbox", "--as", "api", "--all"], "");
    assert_eq!((all["open"].as_u64(), all["items"][0]["state"].as_str()), (Some(0), Some("done")), "{all}");
    assert_eq!(all["items"][0]["status"].as_str(), Some("Added eta_minutes (v2); plan in docs/plans/orders.md"));
    let mine = wait_until(30, "the status at B", || {
        let v = json(&b, &["mail", "sent", "--as", "web"], "");
        (v["sent"][0]["state"] == "closed").then_some(v)
    });
    assert_eq!(mine["sent"][0]["replies"].as_array().map(Vec::len), Some(1), "{mine}");
    // api's own final reply is no open mail of api's: answered.
    let api_sent = json(&a, &["mail", "sent", "--as", "api"], "");
    assert_eq!((api_sent["sent"][0]["state"].as_str(), api_sent["sent"][0]["open"].as_bool()), (Some("answered"), Some(false)), "{api_sent}");
    let inbox_b = json(&b, &["mail", "inbox", "--as", "web"], "");
    assert_eq!((inbox_b["open"].as_u64(), inbox_b["items"].as_array().map(Vec::len)), (Some(0), Some(0)), "nothing for web to close: {inbox_b}");

    // What watch and inbox showed is seen: check agrees with them (nothing open, nothing new).
    let c = run(&b, None, &["mail", "check", "--as", "web"], "");
    assert!(c.code == 0 && c.stdout.starts_with("Seedwell mail for web: nothing open, nothing new"), "{}", c.stdout);
    let again = run(&b, None, &["mail", "check", "--new", "--as", "web"], "");
    assert_eq!((again.code, again.stdout.as_str()), (0, ""), "nothing new since it last told");
    let none = run(&a, None, &["mail", "check", "--as", "api"], "");
    assert!(none.code == 0 && none.stdout.starts_with("Seedwell mail for api: nothing open, nothing new"), "api has nothing open: {}", none.stdout);

    // watch --for: web asks again; api answers without closing; web's watch gets just that answer.
    let q = json(&b, &["mail", "send", "api", "--as", "web", "--type", "question", "-"], "Integer minutes, or a duration string?");
    let qid = q["id"].as_str().unwrap().to_owned();
    let mut wait_for = Command::new(BIN)
        .arg("--socket")
        .arg(&b.socket)
        .args(["--json", "mail", "watch", "--for", &qid, "--from", "api", "--timeout", "60", "--as", "web"])
        .env_remove("SEEDWELL_MAIL_CHANNEL")
        .current_dir(b.state.parent().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    wait_until(30, "the question at A", || (json(&a, &["mail", "inbox", "--as", "api"], "")["open"] == 1).then_some(()));
    let r2 = json(&a, &["mail", "reply", &qid, "--as", "api", "-"], "Integer minutes.");
    assert_eq!(r2["final"].as_bool(), Some(false));
    let lines: Vec<String> = BufReader::new(wait_for.stdout.take().unwrap()).lines().map_while(Result::ok).collect();
    assert!(wait_for.wait().unwrap().success());
    assert_eq!(lines.len(), 1, "{lines:?}");
    let got: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!((got["re"].as_str(), got["news"].as_str(), got["text"].as_str()), (Some(qid.as_str()), Some("item"), Some("Integer minutes.")), "{got}");
    let open_b = json(&b, &["mail", "inbox", "--as", "web"], "");
    assert_eq!(open_b["open"].as_u64(), Some(1), "a reply without --done waits for web to close it: {open_b}");
    // web answers the answer with a final reply: a thread of three mails (and the statuses), exported as one record.
    let r3 = json(&b, &["mail", "reply", got["id"].as_str().unwrap(), "--as", "web", "--done", "Using integer minutes.", "-"], "Thanks: integer minutes it is.");
    assert_eq!(r3["final"].as_bool(), Some(true));
    let md = wait_until(30, "the three mails at A", || {
        let o = run(&a, None, &["mail", "export", &qid, "--as", "api"], "");
        (o.stdout.matches("\n## ").count() == 3).then_some(o.stdout)
    });
    assert!(md.starts_with("# Integer minutes, or a duration string?\n") && md.contains("- **Between:** web, api"), "{md}");
    assert!(md.find("Integer minutes, or a duration").unwrap() < md.find("Integer minutes.").unwrap() && md.find("Integer minutes.").unwrap() < md.find("Thanks: integer").unwrap(), "{md}");
    assert!(md.contains(" · closed by web, "), "{md}");
    // At api, web's final reply and its status count once.
    let c = run(&a, None, &["mail", "check", "--as", "api"], "");
    assert!(c.stdout.starts_with("Seedwell mail for api: 1 open; 1 final answer to what you sent."), "{}", c.stdout);
    let timeout = run(&b, None, &["mail", "watch", "--for", &qid, "--from", "nobody", "--timeout", "2", "--as", "web"], "");
    assert_eq!(timeout.code, 6, "{}", timeout.stderr);

    // General: mail inbox says what is new there, without taking it from chat read.
    a.json(&["chat", "send", "Koding", "--as", "docs", "Release on Friday."]);
    let general = wait_until(30, "General at B", || {
        let v = json(&b, &["mail", "inbox", "--as", "web"], "");
        (v["general_new"] == 1).then_some(v)
    });
    assert_eq!(general["general_new"], 1);
    assert_eq!(json(&b, &["mail", "inbox", "--as", "web"], "")["general_new"], 1, "a peek leaves it unread");
    let read = json(&b, &["chat", "read", "Koding", "--topic", "general", "--unread", "--as", "web"], "");
    assert_eq!(read["messages"].as_array().map(Vec::len), Some(1), "{read}");
    assert_eq!(json(&b, &["mail", "inbox", "--as", "web"], "")["general_new"], 0);

    // Chat's lean output understands mail.
    let lean = run(&b, None, &["chat", "read", "Koding", "--project", "api", "--lean", "--as", "web"], "");
    let lean: Value = serde_json::from_str(lean.stdout.trim()).unwrap();
    let first = &lean["messages"][0];
    assert_eq!((first["mail"]["type"].as_str(), first["mail"]["to"].as_str()), (Some("request"), Some("api")), "{first}");
    assert_eq!(first["text"].as_str(), Some("## Add eta_minutes\n\nThe order page shows an ETA."));

    // A file goes with a mail, checked when it is saved; the thread exports as Markdown.
    let file = root.path().join("orders.py");
    std::fs::write(&file, "ETA = 'eta_minutes'\n").unwrap();
    let f = json(&b, &["mail", "send", "api", "--as", "web", "--type", "feedback", "--attach", file.to_str().unwrap(), "-"], "## The client\n\nAttached.");
    let fid = f["id"].as_str().unwrap().to_owned();
    assert_eq!(f["attached"], 1);
    let to = root.path().join("from-web");
    let got = wait_until(30, "the file at A", || {
        let o = run(&a, None, &["mail", "read", &fid, "--as", "api", "--save", to.to_str().unwrap()], "");
        (o.code == 0).then(|| serde_json::from_str::<Value>(o.stdout.trim()).unwrap())
    });
    assert_eq!(got["attachments"][0]["name"], "orders.py");
    assert_eq!(got["text"].as_str(), Some("## The client\n\nAttached."), "the file is listed, not in the text");
    assert_eq!(std::fs::read_to_string(to.join("orders.py")).unwrap(), "ETA = 'eta_minutes'\n");
    let big = root.path().join("big.txt");
    std::fs::write(&big, "x".repeat(20_000)).unwrap();
    let o = run(&b, None, &["mail", "send", "api", "--as", "web", "--attach", big.to_str().unwrap(), "-"], "too big");
    assert!(o.code == 2 && o.stderr.contains("--pass FILE"), "{}", o.stderr);
    let md = run(&a, None, &["mail", "export", &id, "--as", "api"], "");
    assert!(md.stdout.starts_with("# Add eta_minutes\n") && md.stdout.contains("- **State:** closed") && md.stdout.contains(" · closed by api, "), "{}", md.stdout);

    // done on a closed item adds a newer status: the record updates.
    let again = run(&a, None, &["mail", "done", &id, "--as", "api", "-"], "Also documented in docs/api.md.");
    assert_eq!(again.code, 0, "{}", again.stderr);
    let all = json(&a, &["mail", "inbox", "--as", "api", "--all"], "");
    let first = all["items"].as_array().unwrap().iter().find(|i| i["id"] == id.as_str()).unwrap().clone();
    assert_eq!((first["state"].as_str(), first["status"].as_str()), (Some("done"), Some("Also documented in docs/api.md.")), "{all}");

    // Identity: from its own folder ("a", no repository, a name no project has) a post is refused; a read only warns.
    let o = run(&a, None, &["mail", "send", "web", "-"], "who am I?");
    assert!(o.code == 2 && o.stderr.contains("You would post as a (no git repository here, the folder ") && o.stderr.contains("known: "), "{}", o.stderr);
    let o = run(&a, None, &["mail", "inbox"], "");
    assert!(o.code == 0 && o.stderr.contains("Note: You would post as a"), "{}", o.stderr);
    // The answer web's watch showed was seen: api's sent says so.
    let api_sent = json(&a, &["mail", "sent", "--as", "api"], "");
    let answered = api_sent["sent"].as_array().unwrap().iter().find(|s| s["state"] == "answered").unwrap().clone();
    assert!(answered["seen_at"].is_u64(), "{answered}");
    // sent <id> takes any id of the thread (here the question web asked) and gives the whole thread.
    let th = json(&a, &["mail", "sent", &qid, "--as", "api"], "");
    assert!(th["thread"].as_array().unwrap().len() >= 3, "{th}");
    // Plain inbox: the commands once, and General said either way.
    let plain = Command::new(BIN).arg("--socket").arg(&b.socket).args(["mail", "inbox", "--as", "web"]).env_remove("SEEDWELL_MAIL_CHANNEL").output().unwrap();
    let text = String::from_utf8_lossy(&plain.stdout);
    assert!(text.contains("General (the channel's open conversation): "), "{text}");
    assert!(text.matches(" mail ").count() <= 3, "the binary named once per hint line: {text}");
    let projects = json(&a, &["mail", "projects", "--as", "api"], "");
    let names: Vec<&str> = projects["projects"].as_array().unwrap().iter().filter_map(|p| p["name"].as_str()).collect();
    assert!(names.contains(&"api") && names.contains(&"web"), "{projects}");

    // A Seedwell that is not running: check stays silent and still exits 0.
    let gone = Command::new(BIN).args(["--socket", "/nonexistent/control.sock", "mail", "check", "--as", "api"]).output().unwrap();
    assert_eq!((gone.status.code(), gone.stdout.len()), (Some(0), 0));
}

#[test]
fn the_hook_goes_into_the_projects_local_settings_and_out_again() {
    let root = tempfile::tempdir().unwrap();
    let a = Daemon::start(root.path(), "a", "full");
    let project = root.path().join("api");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(project.join("src")).unwrap();
    let file = project.join(".claude").join("settings.local.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, r#"{"permissions":{"allow":["Bash(ls)"]}}"#).unwrap();
    let inside = project.join("src");
    let o = run(&a, Some(&inside), &["mail", "hook", "install"], "");
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let cmd = v["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap().to_owned();
    assert!(cmd.contains(" mail check --as api ") && cmd.contains("--socket"), "named after the repository, for this socket: {cmd}");
    assert!(v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"].as_str().unwrap().contains("mail check --new"), "{v}");
    assert_eq!(v["permissions"]["allow"][0], "Bash(ls)", "the rest is kept");
    assert_eq!(run(&a, Some(&inside), &["mail", "hook", "install"], "").code, 0);
    let twice: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(v, twice, "installing again changes nothing");
    let show = run(&a, Some(&inside), &["mail", "hook", "show"], "");
    let show: Value = serde_json::from_str(show.stdout.trim()).unwrap();
    assert_eq!(show["installed"], true);
    assert!(show["codex"].as_str().unwrap().contains("mail check --as api"), "{show}");
    assert_eq!(run(&a, Some(&inside), &["mail", "hook", "uninstall"], "").code, 0);
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(after, serde_json::json!({"permissions":{"allow":["Bash(ls)"]}}), "only ours is taken out");
    std::fs::write(&file, "not json").unwrap();
    assert_ne!(run(&a, Some(&inside), &["mail", "hook", "install"], "").code, 0, "a file it cannot read is left alone");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "not json");
}

/// Files too big for a mail pass through the synced folder that goes with the channel (§7G.10): the recipient takes
/// them, which removes them on both computers, and `mail passes` finds what is left over.
#[test]
fn a_big_file_passes_through_the_synced_folder_and_is_gone_once_received() {
    let root = tempfile::tempdir().unwrap();
    let a = Daemon::start(root.path(), "a", "full");
    let b = Daemon::start(root.path(), "b", "full");
    let docs = root.path().join("a-Team");
    std::fs::create_dir_all(&docs).unwrap();
    a.json(&["sync", "add", docs.to_str().unwrap()]);
    let link = a.json(&["sync", "share", "a-Team", "--chat", "new"])["link"].as_str().unwrap().to_owned();
    let team_b = root.path().join("b-Team");
    b.json(&["sync", "join", &link, "--to", team_b.to_str().unwrap()]);
    let port = a.json(&["sync", "list"])["port"].as_u64().unwrap();
    b.json(&["sync", "address", "b-Team", &format!("127.0.0.1:{port}")]);
    wait_until(60, "B a member of the folder and the channel", || {
        let ok = b.json(&["chat", "list"])["channels"][0]["role"].is_string() && b.json(&["sync", "list"])["folders"][0]["role"].is_string();
        ok.then_some(())
    });
    for d in [&a, &b] {
        let ch = d.json(&["chat", "list"])["channels"][0]["id"].as_str().unwrap().to_owned();
        d.json(&["chat", "agents", &ch, "post"]);
    }
    // web (on B) makes its inbox known, so api's mail goes to a project that exists.
    json(&b, &["mail", "inbox", "--as", "web"], "");

    let big = root.path().join("graph.bin");
    std::fs::write(&big, vec![5u8; 64 * 1024]).unwrap();
    assert_eq!(run(&a, None, &["mail", "send", "web", "--as", "api", "--attach", big.to_str().unwrap(), "-"], "too big").code, 2, "too big to attach");
    let sent = json(&a, &["mail", "send", "web", "--as", "api", "--type", "heads-up", "--pass", big.to_str().unwrap(), "-"], "## The new graph\n\nIt is passed.");
    let id = sent["id"].as_str().unwrap().to_owned();
    let rel = sent["passed"]["path"].as_str().unwrap().to_owned();
    assert!(rel.starts_with("Seedwell Pass-Through/web/") && rel.ends_with(" from api"), "{sent}");
    assert!(docs.join(&rel).join("graph.bin").exists());
    assert!(docs.join("Seedwell Pass-Through/README.txt").exists());
    assert_eq!(json(&a, &["mail", "passes", "--as", "api"], "")["passes"].as_array().map(Vec::len), Some(1));

    // B's Seedwell marks the pass arrived once it has all of it; the mark syncs back, so api sees it before web looks.
    let state = wait_until(60, "arrived, seen from A", || {
        let v = json(&a, &["mail", "sent", "--as", "api"], "");
        let p = v["sent"].as_array().unwrap().iter().find(|m| m["id"] == id.as_str()).and_then(|m| m["pass"].as_str()).unwrap_or_default().to_owned();
        p.contains(": arrived ").then_some(p)
    });
    assert!(state.ends_with("web has not taken it yet"), "{state}");
    let states: Vec<String> = json(&a, &["mail", "passes", "--as", "api"], "")["passes"][0]["statuses"].as_array().unwrap().iter().map(|s| s["state"].as_str().unwrap().to_owned()).collect();
    assert_eq!(states, ["sent", "arrived"], "the sender's, then the receiver's");
    // Only its recipient takes it; web waits for the folder to bring it.
    wait_until(30, "the mail at B", || (json(&b, &["mail", "inbox", "--as", "web"], "")["open"] == 1).then_some(()));
    let out = root.path().join("got");
    let o = run(&b, None, &["mail", "receive", &id, "--to", out.to_str().unwrap(), "--as", "api"], "");
    assert_eq!(o.code, 2, "api is not its recipient: {}", o.stderr);
    let got = json(&b, &["mail", "receive", &id, "--to", out.to_str().unwrap(), "--wait", "60s", "--as", "web"], "");
    assert_eq!(got["received"].as_array().map(Vec::len), Some(1), "{got}");
    assert_eq!(std::fs::read(out.join("graph.bin")).unwrap().len(), 64 * 1024);
    assert!(!team_b.join(&rel).exists(), "gone at B");
    wait_until(60, "gone at A too", || (!docs.join(&rel).exists()).then_some(()));
    assert!(std::fs::read_dir(docs.join(".seedwell/Archive")).map_or(true, |d| d.count() == 0), "and not archived");
    assert!(json(&a, &["mail", "passes", "--as", "api"], "")["passes"].as_array().unwrap().is_empty());
    let v = json(&a, &["mail", "sent", "--as", "api"], "");
    let p = v["sent"].as_array().unwrap().iter().find(|m| m["id"] == id.as_str()).and_then(|m| m["pass"].as_str()).unwrap_or_default().to_owned();
    assert!(p.starts_with("pass: re"), "received, or removed: {p}");
    let o = run(&b, None, &["mail", "receive", &id, "--to", out.to_str().unwrap(), "--as", "web"], "");
    assert!(o.code == 2 && o.stderr.contains("already"), "{}", o.stderr);
    // The sender sees it was received.
    wait_until(30, "received, seen from A", || {
        let t = json(&a, &["mail", "tag", &id, "--as", "api"], "");
        t["tags"].as_array().unwrap().iter().any(|o| o["owner"] == "web" && o["tags"].as_array().unwrap().iter().any(|x| x == "received")).then_some(())
    });
}
