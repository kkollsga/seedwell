# dmgbuild settings for Seedwell's disk image: the app, a link to /Applications beside it, a background with an arrow
# between them (make_background.swift draws it for this layout) and the app icon as the volume's icon.
#
#     dmgbuild -s app/dmg/settings.py -D app=<Seedwell.app> -D background=<background.tiff> -D icon=<AppIcon.icns> \
#         "Seedwell X.Y.Z" Seedwell-X.Y.Z.dmg
#
# `defines` is dmgbuild's: the -D values. The numbers below and make_background.swift's are the same layout.
import os.path

app = defines["app"]  # noqa: F821 (dmgbuild provides `defines`)
background = defines["background"]  # noqa: F821
icon = defines["icon"]  # noqa: F821

# Read-only, compressed with LZFSE (macOS 10.11 and later; Seedwell needs 15), HFS+ for the widest Finder support.
format = "ULFO"
filesystem = "HFS+"
files = [app]
symlinks = {"Applications": "/Applications"}
# No hide_extensions: it sets a Finder flag on the app bundle, which its code signature does not allow (codesign
# --verify --strict refuses the app on the disk image). The Finder hides ".app" by itself.

# The window: no toolbar, sidebar, path or status bar; icon view at 128 pt; its content is the background's size.
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
default_view = "icon-view"
window_rect = ((200, 160), (640, 400))
icon_size = 128
text_size = 13
arrange_by = None
icon_locations = {
    os.path.basename(app): (170, 190),
    "Applications": (470, 190),
}
