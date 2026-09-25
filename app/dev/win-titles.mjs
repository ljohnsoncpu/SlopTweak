// Dev helper: titles of a process's visible top-level windows. The Invoke
// window's title carries the cost bar (the remote page can't show app UI).
import { execSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const PS1 = join(mkdtempSync(join(tmpdir(), "sloptweak-titles-")), "titles.ps1");
writeFileSync(
  PS1,
  `param([int]$ProcId)
# UTF-8, or titles with "—" and "·" come back mangled in the OEM codepage.
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Add-Type -Name W -Namespace U -MemberDefinition @'
public delegate bool EnumProc(System.IntPtr h, System.IntPtr l);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc f, System.IntPtr l);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(System.IntPtr h, out uint p);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr h);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(System.IntPtr h, System.Text.StringBuilder s, int n);
'@
$titles = New-Object System.Collections.Generic.List[string]
[void][U.W]::EnumWindows({ param($h, $l) $p = 0; [void][U.W]::GetWindowThreadProcessId($h, [ref]$p)
  if ($p -eq $ProcId -and [U.W]::IsWindowVisible($h)) { $sb = New-Object System.Text.StringBuilder 512; [void][U.W]::GetWindowText($h, $sb, 512); if ($sb.Length) { $titles.Add($sb.ToString()) } }
  $true }, [System.IntPtr]::Zero)
$titles
`,
);

export function windowTitles(pid) {
  const out = execSync(`powershell -NoProfile -ExecutionPolicy Bypass -File "${PS1}" -ProcId ${pid}`, {
    encoding: "utf8",
  });
  return out
    .split(String.fromCharCode(10))
    .map((l) => l.trim())
    .filter(Boolean);
}

export const COST_TITLE = /^SlopTweak — Invoke · \$[\d.]+\/hr · .+ · ≈\$[\d.]+ so far/;
