# Writes Ctrl+D key down and up records to the console of process $env:PI_CONSOLE_ATTACH_PID.
# Start it detached so PowerShell never owns that console. Used by crates/pi-tui/tests.
$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

public static class PiConsoleKeys {
    [StructLayout(LayoutKind.Explicit, CharSet = CharSet.Unicode)]
    struct KeyEventRecord {
        [FieldOffset(0)] public int KeyDown;
        [FieldOffset(4)] public ushort RepeatCount;
        [FieldOffset(6)] public ushort VirtualKeyCode;
        [FieldOffset(8)] public ushort VirtualScanCode;
        [FieldOffset(10)] public char UnicodeChar;
        [FieldOffset(12)] public uint ControlKeyState;
    }

    [StructLayout(LayoutKind.Explicit, CharSet = CharSet.Unicode)]
    struct InputRecord {
        [FieldOffset(0)] public ushort EventType;
        [FieldOffset(4)] public KeyEventRecord KeyEvent;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool FreeConsole();
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool AttachConsole(uint processId);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateFileW(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool WriteConsoleInputW(IntPtr input, InputRecord[] records, uint count, out uint written);
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr handle);

    const ushort KeyEventType = 1;
    const ushort VirtualKeyD = 0x44;
    const ushort ScanCodeD = 0x20;
    const uint LeftCtrlPressed = 0x0008;

    public static void SendCtrlD(uint processId) {
        FreeConsole();
        if (!AttachConsole(processId)) throw new Win32Exception();
        IntPtr input = CreateFileW("CONIN$", 0xC0000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
        if (input == new IntPtr(-1)) throw new Win32Exception();
        try {
            InputRecord[] records = new InputRecord[2];
            for (int i = 0; i < records.Length; i++) {
                records[i].EventType = KeyEventType;
                records[i].KeyEvent.KeyDown = i == 0 ? 1 : 0;
                records[i].KeyEvent.RepeatCount = 1;
                records[i].KeyEvent.VirtualKeyCode = VirtualKeyD;
                records[i].KeyEvent.VirtualScanCode = ScanCodeD;
                records[i].KeyEvent.UnicodeChar = '\u0004';
                records[i].KeyEvent.ControlKeyState = LeftCtrlPressed;
            }
            uint written;
            if (!WriteConsoleInputW(input, records, (uint)records.Length, out written)) throw new Win32Exception();
            if (written != records.Length) throw new Exception("wrote " + written + " of " + records.Length + " records");
        } finally {
            CloseHandle(input);
            FreeConsole();
        }
    }
}
'@

if (-not $env:PI_CONSOLE_ATTACH_PID) { throw 'PI_CONSOLE_ATTACH_PID is not set' }
[PiConsoleKeys]::SendCtrlD([uint32]$env:PI_CONSOLE_ATTACH_PID)
