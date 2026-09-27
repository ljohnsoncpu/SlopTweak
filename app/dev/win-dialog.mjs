// Dev helper: close a process's window by title (like its X button) and
// press a button on a native dialog (TaskDialog), both with window
// messages, so no focus or mouse is needed.
import { execSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const PS1 = join(mkdtempSync(join(tmpdir(), "sloptweak-dialog-")), "dialog.ps1");
writeFileSync(
  PS1,
  `param([int]$ProcId, [string]$TitleB64, [string]$Action, [int]$Button = 0)
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
# Base64 so "—" survives the command line.
$Title = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String($TitleB64))
Add-Type -Name D -Namespace U -MemberDefinition @'
public delegate bool EnumProc(System.IntPtr h, System.IntPtr l);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc f, System.IntPtr l);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(System.IntPtr h, out uint p);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr h);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(System.IntPtr h, System.Text.StringBuilder s, int n);
[DllImport("user32.dll")] public static extern bool PostMessage(System.IntPtr h, uint m, System.IntPtr w, System.IntPtr l);
'@
$found = [System.IntPtr]::Zero
[void][U.D]::EnumWindows({ param($h, $l) $p = 0; [void][U.D]::GetWindowThreadProcessId($h, [ref]$p)
  if ($p -eq $ProcId -and [U.D]::IsWindowVisible($h)) { $sb = New-Object System.Text.StringBuilder 512; [void][U.D]::GetWindowText($h, $sb, 512)
    if ($sb.ToString().StartsWith($Title)) { $script:found = $h; return $false } }
  $true }, [System.IntPtr]::Zero)
if ($found -eq [System.IntPtr]::Zero) { "none"; exit }
# WM_CLOSE = 0x10; TDM_CLICK_BUTTON = WM_USER + 102 = 0x466.
if ($Action -eq "close") { [void][U.D]::PostMessage($found, 0x10, [System.IntPtr]::Zero, [System.IntPtr]::Zero) }
else { [void][U.D]::PostMessage($found, 0x466, [System.IntPtr]$Button, [System.IntPtr]::Zero) }
"ok"
`,
);

function run(pid, title, action, button = 0) {
  const t = Buffer.from(title, "utf8").toString("base64");
  return execSync(
    `powershell -NoProfile -ExecutionPolicy Bypass -File "${PS1}" -ProcId ${pid} -TitleB64 ${t} -Action ${action} -Button ${button}`,
    { encoding: "utf8" },
  ).trim();
}

/** Ask the window whose title starts with `title` to close. "ok" or "none". */
export const closeWindow = (pid, title) => run(pid, title, "close");

export const IDYES = 6;
export const IDNO = 7;
export const IDCANCEL = 2;
/** Press a TaskDialog button (IDYES, IDNO, IDCANCEL). "ok" or "none". */
export const pressDialog = (pid, title, id) => run(pid, title, "press", id);
