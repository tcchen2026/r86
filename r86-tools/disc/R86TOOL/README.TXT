r86-tool for Windows 95 / NT 4 and later

r86-tool install                  copy this EXE, README and LICENSE to C:\R86;
                                  register for all users at logon; start helper
r86-tool uninstall                stop helper; remove its Run entry and files
r86-tool driver display [FOLDER]  install this OS's display driver
r86-tool version                  print tool and protocol versions
r86-tool autorun                  the logon helper: absolute pointer with a
                                  taskbar icon (Settings, pointer, Exit)
r86-tool modes                    list display modes
r86-tool mode WIDTH HEIGHT [BITS] change and save mode (16 bits by default)
r86-tool mouse                    run the hidden absolute pointer helper
r86-tool mouse stop               stop it and release held buttons
r86-tool mouse install            legacy per-user startup for this copy
r86-tool mouse remove             remove that legacy entry and stop helper

With no arguments, a copy outside C:\R86 (the tools CD, which Windows opens by
itself when the disc is inserted) shows the installer window: choose the
helper and the display driver, then click Install (Update when C:\R86 already
has a copy). C:\R86\R86-TOOL.EXE with no arguments opens Settings: screen
resolution, including modes the Display control panel hides, the absolute
mouse, and starting at logon. Clicking the taskbar icon opens the same window;
right-click it to turn the pointer off or exit. On the command line, install
installs the helper; driver display is a separate step. r86-tool is a GUI
program: output goes to a redirected file, the console it was started from
(XP), or a new console that waits for Enter. cmd.exe does not wait for it; use
start /wait for the result. On NT, installation/removal requires an
administrator. Result 0 means success; 3010 means restart Windows. No command
reboots for you. Uninstall from the installed copy can defer EXE deletion to
the next boot; uninstall from the CD removes it immediately. Unrelated C:\R86
files remain.

Windows 95/98/Me: attach the r86 tools CD for VMDisp9x. Windows 95 also needs
the Microsoft SetupAPI redistributable; it is not bundled here.
Windows 2000/XP: provide the original extracted VBEMP VBE20 package on a CD
or pass its root or the directory containing vbemppnp.inf and vbemp.sys.
VBEMP is not on the public tools CD. See https://r86.run/vbemp. Use 16-bit
colour; 256 colours can crash winlogon. NT 4 display setup remains manual.

r86's web console (View, Guest resolution) asks the running helper to switch
the display mode; the helper reports the modes Windows offers. On Windows
95/98/Me with VMDisp9x, Settings and the console's Add custom resolution add a
size to the driver's mode list (for example the handheld panel's 320x170 or
640x340); Windows offers it after a restart. Sizes start at 320x170, which
needs this CD's VMDisp9x (stock VMDisp9x starts at 640x480): Update with the
display driver ticked. Updating the driver keeps added sizes.

One helper instance runs per Windows session. On NT secure/logon desktops it
does not poll or inject input, allowing the frontend's PS/2 fallback. Wheel
input requires Windows 98 / NT 4.
Source: r86-tools/r86-tool.c; build: script/build_tools_iso.py. Protocol:
src/machine/magic.rs.
License: LICENSE.TXT.
