#!/usr/bin/env bash
# The torrent proxy (SPEC §7H) end to end in the app, against a loopback SOCKS5 fake (fake_socks5.py) that the isolated
# run's core uses in place of NordVPN's servers (<state>/qa/proxy-servers.txt): NordVPN is never contacted. Own QA root
# (seedwell-qa-proxy). Every state of the shield (no key, off, on, slow, failed) is reached through the QA commands the
# clicks make (`proxy …`, QAProxy): the paste-friendly fields (QA's own pasteboard), Enable NordVPN with its
# "Testing…", a click on the shield in Settings and in the menu, the right-click menu and its country list (6 rows with
# a scrollbar, in the menu bar's popover and the window's), Find a Faster Server, a wrong key, a custom proxy and
# Remove Key. Shots, dark and light, into $QA_OUT (default app/build/qa): proxy-<state>-settings|menu|window|guide-<mode>.png,
# proxy-country-menu|window-<mode>.png and a few of single moments.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-proxy"
source "$(dirname "$0")/env.sh"
qa_fresh
qa_write_config
PORT=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
mkdir -p "$STATE/qa"
printf '127.0.0.1:%s NL Amsterdam\n[::1]:%s SE Stockholm\n' "$PORT" "$PORT" > "$STATE/qa/proxy-servers.txt"
delay() { echo "$2" > "$QA_ROOT/socks-delay-$1"; }
fake_start() {
  # "$QA_ROOT/" with the slash: qa_teardown ends every process whose command line names "$QA_ROOT/", so the fake
  # never outlives the run (without it, it kept listening after the script ended).
  detach python3 "$REPO/app/qa/fake_socks5.py" "$QA_ROOT/" "$PORT" svc-user svc-pass >>"$QA_ROOT/socks.log" 2>&1 &
  qa_until 10 qa_listening "$PORT" || { echo "the fake SOCKS5 proxy did not start"; FAIL=1; }
}
fake_stop() { pkill -f "fake_socks5.py $QA_ROOT/ " 2>/dev/null; qa_until 10 sh -c "! lsof -nP -iTCP:$PORT -sTCP:LISTEN -t >/dev/null 2>&1"; }
delay v4 0; delay v6 0
fake_start
qa_app_start --window-size 1180x820 || qa_end proxy

# The shield's three places, dark and light: Settings › Network, the menu bar menu, the window (its guide hidden and
# shown).
shots() {
  local state=$1 mode
  for mode in dark light; do
    qa_cmd "appearance $mode" "window open" "window size 1180x820" "settings network"
    qa_shot "proxy-$state-settings-$mode" settings
    qa_cmd "menu open"
    qa_shot "proxy-$state-menu-$mode" menu
    qa_cmd "menu-hover shield"
    qa_shot "proxy-$state-menu-hover-$mode" menu
    qa_cmd "menu-hover none" "menu close" "proxy open" "proxy guide hide"
    qa_shot "proxy-$state-window-$mode" "window proxy"
    qa_cmd "proxy guide show"
    qa_shot "proxy-$state-guide-$mode" "window proxy"
    qa_cmd "proxy guide hide" "proxy close"
  done
}

echo "== no key"
out=$(qa_ask "proxy model")
check "no key: grey shield" "$out" "icon noKey, key no"
check "no key: a click opens the window" "$out" "click opens the window"
check "the right-click menu offers the window" "$out" "menu [SOCKS5 Proxy…|-|Country: No preference…]"
out=$(qa_ask "proxy click")
check "the click opened the window, toggled nothing" "$out" "icon noKey"
check "the window is open" "$(qa_ask "proxy window")" "proxy-window: open, pane nordVpn"
check "the window is listed" "$(qa_ask windows)" "proxy"
qa_cmd "proxy close"
shots nokey
out=$(qa_ask "proxy context")
check "right-click: SOCKS5 Proxy… first, then the country entry (no submenu)" "$out" "proxy: context SOCKS5 Proxy…|-|Country: No preference…"
qa_shot proxy-nokey-context menu
qa_cmd "contextmenu close"
qa_cmd "proxy press SOCKS5 Proxy…"
check "SOCKS5 Proxy… opens the window" "$(qa_ask "proxy window")" "proxy-window: open"

echo "== copied credentials fill the empty fields while the window is open"
check "the window watches the pasteboard" "$(qa_ask "proxy window")" "watching-pasteboard yes"
out=$(qa_ask "proxy copy svc-user")
check "the first copy fills the username" "$out" "username set, password empty, pasted-note shown"
out=$(qa_ask "proxy copy svc-pass")
check "the second copy, soon after, fills the password" "$out" "username set, password set, pasted-note shown"
out=$(qa_ask "proxy copy something-else")
check "a filled field is never changed" "$out" "username set, password set"
qa_cmd "appearance light"
qa_shot proxy-filled-window-light "window proxy"

echo "== Enable NordVPN: Testing…, then on"
delay v4 1.2; delay v6 1.2
qa_cmd "proxy enable"
out=$(qa_wait "proxy window ~ line progress" 5)
check "the shield and switch wait while the key is tested" "$(qa_ask "proxy model")" "busy yes"
check "Testing… while the key is tested" "$out" "line progress 'Testing the key with NordVPN"
check "the buttons wait" "$out" "busy yes"
qa_shot proxy-testing-window-light "window proxy"
qa_cmd "appearance dark"
qa_shot proxy-testing-window-dark "window proxy"
out=$(qa_wait "proxy window ~ result 'NordVPN is on" 20)
check "the key is kept and the fields are emptied" "$out" "username empty, password empty, pasted-note hidden"
lacks "the key is never shown" "$out" "svc-pass"
check "with the key installed the pane shows it, not the setup form" "$out" "installed 'NordVPN key installed', replacing no, form hidden"
out=$(qa_ask "proxy copy another-user")
check "hidden fields take no copy" "$out" "username empty, password empty"
qa_cmd "proxy replace"
out=$(qa_ask "proxy window")
check "Replace Key… shows the form again" "$out" "replacing yes, form shown"
for mode in dark light; do qa_cmd "appearance $mode"; qa_shot "proxy-replace-window-$mode" "window proxy"; done
out=$(qa_ask "proxy replace cancel")
check "Cancel hides it again" "$out" "replacing no, form hidden"
qa_cmd "proxy close"
qa_wait "proxy model ~ icon on" 20 >/dev/null
delay v4 0.15; delay v6 0
qa_cmd "proxy open"
check "Find a Faster Server shows while it is on" "$(qa_ask "proxy window")" "find-faster shown"
qa_cmd "proxy find-faster"
# (the key test's result said "in 1.2 s": only Find a Faster Server's says "ms).")
out=$(qa_wait "proxy window ~ ms)." 20)
check "Find a Faster Server says what came of it: the server, its place and latency" "$out" "ms)."
for mode in dark light; do qa_cmd "appearance $mode"; qa_shot "proxy-faster-window-$mode" "window proxy"; done
qa_cmd "proxy close"
out=$(qa_wait "proxy model ~ ms'" 20)
check "on: green, the server and its latency" "$out" "icon on, key yes, on yes, switch shown, status 'On · NordVPN · "
check "with a key a click toggles" "$out" "click toggles"
check "with a key the On switch shows" "$out" "switch shown"
lacks "the key is in no model" "$out" "svc-pass"
shots on
out=$(qa_ask "proxy context")
check "right-click with a key: the window, off, faster, country" "$out" "SOCKS5 Proxy…|Turn Off NordVPN|Find a Faster Server|-|Country: No preference…"
qa_cmd "contextmenu close"

echo "== the country list: 6 rows with a scrollbar, the same in the menu and the window"
qa_cmd "proxy press Country: No preference…"
out=$(qa_wait "proxy country-list show ~ open at menuBar" 10)
check "the menu's entry opens the list (not a submenu)" "$out" "open at menuBar"
check "6 rows show" "$out" "showing 6 ("
check "the list is the rows exactly" "$out" "(the rows exactly: yes)"
check "the rest scrolls" "$out" "(scrolls yes)"
check "No preference at the top, checked and shown" "$out" "top row 0 (No preference at 0, shown yes)"
out=$(qa_ask "proxy country-list choose Sweden")
check "choosing a row closes the list" "$out" "proxy-country-list: closed"
out=$(qa_wait "proxy model ~ country SE" 10)
check "the chosen country is kept" "$out" "country SE"
check "the entry names it" "$out" "Country: Sweden…"
out=$(qa_ask "proxy country-list menu")
check "opened again: scrolled to Sweden" "$out" "(Sweden at "
check "opened again: Sweden shows" "$out" "shown yes)"
lacks "opened again: not at the top" "$out" "top row 0 "
for mode in dark light; do qa_cmd "appearance $mode" "proxy country-list menu"; qa_shot "proxy-country-menu-$mode" "window country-list"; done
qa_cmd "proxy country-list close"
qa_cmd "proxy open"
out=$(qa_ask "proxy country-list window")
check "the window's button opens the same list" "$out" "open at window, rows "
check "the window's list: 6 rows, scrolled to Sweden" "$out" "(the rows exactly: yes)"
check "the window's list shows Sweden" "$out" "shown yes)"
for mode in dark light; do qa_cmd "appearance $mode" "proxy country-list window"; qa_shot "proxy-country-window-$mode" "window proxy"; done
out=$(qa_ask "proxy country-list down")
check "Down starts at the chosen row" "$out" "highlighted Sweden"
out=$(qa_ask "proxy country-list type n")
check "a letter jumps to a country with it" "$out" "highlighted N"
out=$(qa_ask "proxy country-list escape")
check "Escape closes it and keeps the country" "$out" "closed, country SE"
qa_cmd "proxy country-list window" "proxy country-list type n" "proxy country-list return"
out=$(qa_wait "proxy model !~ country SE" 10)
lacks "Return chooses the highlighted one" "$out" "country SE"
qa_cmd "proxy country SE" "proxy close"
qa_cmd "proxy context"
qa_shot proxy-on-context menu
qa_cmd "contextmenu close"

echo "== a click turns it off and on, the switch turns it off, the menu's shield turns it on"
out=$(qa_ask "proxy click")
check "the click turned it off" "$out" "icon off, key yes, on no"
out=$(qa_ask "proxy click")
check "the click turned it on" "$out" "on yes"
out=$(qa_ask "proxy switch")
check "the switch turned it off" "$out" "icon off, key yes, on no"
shots off
qa_cmd "menu open"
out=$(qa_ask "proxy menu-press")
check "the menu's shield turned it on" "$out" "on yes"
qa_cmd "menu close"
qa_wait "proxy model ~ icon on" 20 >/dev/null

echo "== slow"
delay v4 1.8; delay v6 1.8
qa_cmd "proxy find-faster"
out=$(qa_wait "proxy model ~ status 'Slow · NordVPN · " 30)
check "slow: orange, the latency over 1.5 s" "$out" "icon slow"
# Find a Faster Server's own test has ended (its "Testing servers…" is not the state).
qa_wait "proxy model !~ Testing servers" 30 >/dev/null
shots slow

echo "== failed (the servers are gone)"
fake_stop
qa_cmd "proxy find-faster"
out=$(qa_wait "proxy model ~ icon failed" 30)
check "failed: the red shield" "$out" "icon failed"
check "failed: the problem in words" "$out" "problem '"
lacks "failed: no system error text" "$out" "os error"
qa_wait "proxy model !~ Testing servers" 30 >/dev/null
shots failed

echo "== a wrong key keeps nothing and says so"
delay v4 0; delay v6 0
fake_start
qa_cmd "proxy open" "proxy replace" "proxy type username me@example.com" "proxy type password my-login-password" "proxy enable"
out=$(qa_wait "proxy window ~ line problem" 20)
check "the Nord Account login is not the key" "$out" "these look like your Nord Account login"
check "the password is emptied after the call" "$out" "password empty"
check "the form stays open to try again" "$out" "replacing yes, form shown"
lacks "the refusal is plain sentences" "$out" "os error"
for mode in dark light; do qa_cmd "appearance $mode"; qa_shot "proxy-badkey-window-$mode" "window proxy"; done

echo "== another SOCKS5 proxy"
qa_cmd "proxy pane custom" "proxy type host 127.0.0.1" "proxy type port $PORT" "proxy type custom-user svc-user" "proxy type custom-password svc-pass" "proxy use"
out=$(qa_wait "proxy window ~ result 'The proxy is on" 20)
check "the custom proxy is tested and kept" "$out" "key-line 'A proxy is set up (127.0.0.1:$PORT)"
check "its pane shows it installed" "$out" "installed 'Proxy installed: 127.0.0.1:$PORT', replacing no, form hidden"
for mode in dark light; do qa_cmd "appearance $mode"; qa_shot "proxy-custom-window-$mode" "window proxy"; done

echo "== Remove Key"
qa_cmd "proxy remove"
out=$(qa_wait "proxy model ~ icon noKey" 10)
check "removed: grey, a click opens the window" "$out" "click opens the window"
check "no key: no switch" "$out" "switch hidden"
check "the window says no key" "$(qa_ask "proxy window")" "key-line 'No key is installed.', installed '', replacing no, form shown, remove-key hidden"
qa_cmd "proxy close" "settings close" "window close"
check "nothing shows the shield: no polling" "$(qa_ask "proxy model")" "polling no"
qa_end proxy
