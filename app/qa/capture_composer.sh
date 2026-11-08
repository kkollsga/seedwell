#!/usr/bin/env bash
# The chat composer end to end in the running app (own QA root seedwell-qa-composer): a channel made through the CLI,
# then typing, Shift- and Option-Return, an input method's composition that Return commits, a paste of text and of a
# picture, a drop before a character, Reply (the field takes the keyboard) and Escape, Return sending; `chat state`
# after each step, the message through `seedwell chat read`. Every step is the call its key, paste or drop makes
# (`chat …` QA commands, no event is posted). Shots: app/build/qa/composer-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-composer"
source "$(dirname "$0")/env.sh"
qa_fresh
qa_write_config
qa_app_start --window-size 1180x820 || qa_end composer
cli() { "$CLI" --socket "$SOCK" --client claude-code "$@"; }
if ! out=$(cli chat new "QA Composer" 2>&1); then echo "  FAIL: chat new: $out"; FAIL=1; qa_end composer; fi
python3 -I -c 'import sys, struct, zlib
w, h = 8, 6
raw = b"".join(b"\0" + b"\x30\x80\xd0\xff" * w for _ in range(h))
chunk = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))
open(sys.argv[1], "wb").write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))' "$QA_ROOT/dot.png"

qa_cmd "window open" "chat open QA Composer"
s=$(qa_ask "chat state"); check "empty field" "$s" 'text "" · caret 0+0'; check "placeholder" "$s" "placeholder yes"
check "one line" "$s" "height 1 lines"
s=$(qa_ask "chat type Hello"); check "typed" "$s" 'text "Hello" · caret 5+0'; check "no placeholder" "$s" "placeholder no"
s=$(qa_ask "chat key shift-enter"); check "Shift-Return: a line" "$s" 'text "Hello\n"'
s=$(qa_ask "chat key option-enter"); check "Option-Return: a line" "$s" 'text "Hello\n\n"'
s=$(qa_ask "chat compose にほん"); check "composing" "$s" "composing yes"
s=$(qa_ask "chat key enter"); check "Return commits the composition" "$s" 'text "Hello\n\nにほん" · caret 10+0'
check "not sent" "$s" "composing no"
s=$(qa_ask "chat paste text  world"); check "paste text" "$s" 'text "Hello\n\nにほん world"'
s=$(qa_ask "chat drop $QA_ROOT/dot.png at 3"); check "drop at 3" "$s" "chat: drop at 3 taken"
s=$(qa_ask "chat state"); check "reference where it landed" "$s" 'text "Hel ![Image 1](att:1) lo\n\nにほん world"'
check "marker" "$s" "markers 1@4+17"; check "attached" "$s" "attachments dot.png"
s=$(qa_ask "chat drop $QA_ROOT/dot.png chips"); check "drop on the chips" "$s" "chat: drop chips taken"
qa_cmd "chat select 0"
s=$(qa_ask "chat paste png $QA_ROOT/dot.png"); check "paste a picture at the start" "$s" 'text "![Image 3](att:3) Hel ![Image 1](att:1) lo'
qa_wait "chat state ~ attachments dot.png,dot.png," 10 >/dev/null || { echo "  FAIL: three attachments"; FAIL=1; }
s=$(qa_ask "chat state"); check "height grows" "$s" "height 3 lines"
qa_shot composer-draft main
# A message from the CLI to reply to (an agent's), then Reply on it and Escape.
cli chat send "QA Composer" "Reply to this" >/dev/null 2>&1 || { echo "  FAIL: chat send"; FAIL=1; }
qa_cmd "chat open QA Composer"
qa_wait "chat messages ~ Reply to this" 10 >/dev/null || { echo "  FAIL: the message is not shown"; FAIL=1; }
id=$(qa_ask "chat messages" | grep "Reply to this" | tail -1 | cut -d' ' -f1)
s=$(qa_ask "chat reply $id"); check "reply bar" "$s" "chat: reply"
s=$(qa_wait "chat state ~ focus yes" 5); check "Reply: the field has the keyboard" "$s" "reply $id"
qa_shot composer-reply main
s=$(qa_ask "chat key escape"); check "Escape cancels the reply" "$s" "reply none"
check "and keeps the text" "$s" '![Image 3](att:3) Hel ![Image 1](att:1) lo'
# Return sends everything: the field empties and the message is in the channel.
s=$(qa_ask "chat key enter")
qa_wait "chat state ~ placeholder yes" 10 >/dev/null || { echo "  FAIL: the field did not empty after Return"; FAIL=1; }
qa_until 15 sh -c "\"$CLI\" --socket \"$SOCK\" chat read \"QA Composer\" 2>/dev/null | grep -q 'にほん world'" \
  || { echo "  FAIL: the message is not in the channel"; FAIL=1; }
qa_shot composer-sent main

# A message's own controls, as their pointer and clicks call them: a code block's copy button (into QA's own
# pasteboard), the pointer's Tag and Reply, a gallery's arrows and thumbnails, a click on an image reference.
cp "$QA_ROOT/dot.png" "$QA_ROOT/dot2.png"
printf 'Run it:\n\n```sh\nseedwell status --json\n```\n' | cli chat send "QA Composer" - >/dev/null 2>&1 || { echo "  FAIL: send code"; FAIL=1; }
cli chat send "QA Composer" --attach "$QA_ROOT/dot.png" --attach "$QA_ROOT/dot2.png" 'Two dots: the second ![Image 2](att:2)' >/dev/null 2>&1 \
  || { echo "  FAIL: send images"; FAIL=1; }
qa_cmd "chat open QA Composer"
qa_wait "chat messages ~ Two dots" 10 >/dev/null || { echo "  FAIL: the gallery message is not shown"; FAIL=1; }
code=$(qa_ask "chat messages" | grep "Run it" | tail -1 | cut -d' ' -f1)
dots=$(qa_ask "chat messages" | grep "Two dots" | tail -1 | cut -d' ' -f1)
s=$(qa_ask "chat copy-code $code"); check "copy button: the code exactly" "$s" 'chat: copied exactly 22 characters: seedwell status --json'
s=$(qa_ask "chat hover $code"); check "the pointer's controls" "$s" "hover $code on · reply Reply"
qa_shot composer-hover main
qa_cmd "chat hover $code off"
s=$(qa_ask "chat gallery $dots state"); check "gallery" "$s" "1 / 2"
s=$(qa_ask "chat gallery $dots next"); check "→" "$s" "2 / 2"
s=$(qa_ask "chat gallery $dots next"); check "→ at the end stays" "$s" "2 / 2"
s=$(qa_ask "chat gallery $dots prev"); check "←" "$s" "1 / 2"
s=$(qa_ask "chat gallery $dots 2"); check "a thumbnail" "$s" "2 / 2"
qa_cmd "chat gallery $dots 1"
s=$(qa_ask "chat chip $dots 2"); check "a click on the reference shows its image" "$s" "shows 2 / 2"
qa_shot composer-gallery main

# Groups of channels (SPEC 7G.7 Groups), made through the CLI as an agent would: the group above the plain list with
# its icon from its name, its channel under it and not also in the list, its menu, and closed with its count.
cli chat new "QA Plain" >/dev/null 2>&1 || { echo "  FAIL: chat new QA Plain"; FAIL=1; }
cli chat topic "QA Plain" Dinner >/dev/null 2>&1 || { echo "  FAIL: chat topic"; FAIL=1; }
qa_cmd "chat-channel QA Plain open"
qa_cmd "chat-channel QA Plain close"
s=$(qa_ask "chat-groups"); check "a channel's topics closed: no two rows in one place" "$s" "rows ok"
cli chat group new Agents >/dev/null 2>&1 || { echo "  FAIL: chat group new"; FAIL=1; }
cli chat group move "QA Composer" Agents >/dev/null 2>&1 || { echo "  FAIL: chat group move"; FAIL=1; }
s=$(cli chat group list 2>&1); check "the CLI lists the group with its channel" "$s" "1. Agents  [sparkles]  QA Composer"
s=$(qa_wait "chat-groups ~ Agents [Sparkles] open" 10)
check "the sidebar draws the group with its channel" "$s" "open, unread 0: QA Composer"
check "the group's channel is not also in the plain list" "$s" "plain QA Plain"
check "the group's menu" "$s" "menu Rename Group…|Icon ["
check "no two rows in one place" "$s" "rows ok"
qa_shot composer-group-open main
# Closed while its channel is shown: the page stays, the group's row holds the selection (its icon in the accent).
qa_cmd "chat open QA Composer"
qa_cmd "chat-group Agents close"
s=$(qa_wait "chat-groups ~ closed" 10); check "closed, holding the channel shown" "$s" "Agents [Sparkles] closed holding the channel shown"
check "no two rows in one place while it closes" "$s" "rows ok"
s=$(qa_ask "where"); check "the page stays" "$s" "section chat"
qa_shot composer-group-holding main
# Another channel shown: the closed group holds nothing, and the rows that went are not drawn under the ones that came.
qa_cmd "chat open QA Plain"
s=$(qa_ask "chat-groups"); check "closed" "$s" "Agents [Sparkles] closed, unread 0"
check "no two rows in one place" "$s" "rows ok"
qa_shot composer-group-closed main
cli chat group delete Agents >/dev/null 2>&1 || { echo "  FAIL: chat group delete"; FAIL=1; }
s=$(qa_wait "chat-groups ~ chat-groups: none" 10); check "deleted: its channel back in the plain list" "$s" "QA Composer"
qa_end composer
