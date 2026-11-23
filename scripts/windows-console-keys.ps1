# Types $env:PI_CONSOLE_KEYS into the console of $env:PI_CONSOLE_ATTACH_PID as key-down records with
# virtual key 0, which conhost passes through as text in VT input mode. Start it detached.
$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

public static class PiConsoleKeys {
    [StructLayout(LayoutKind.Explicit, Size = 16)]
    struct KeyEventRecord {
        [FieldOffset(0)] public int KeyDown;
        [FieldOffset(4)] public ushort RepeatCount;
        [FieldOffset(6)] public ushort VirtualKeyCode;
        [FieldOffset(8)] public ushort VirtualScanCode;
        [FieldOffset(10)] public ushort UnicodeChar;
        [FieldOffset(12)] public uint ControlKeyState;
    }

    [StructLayout(LayoutKind.Explicit, Size = 20)]
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

    public static void Type(uint processId, string text) {
        FreeConsole();
        if (!AttachConsole(processId)) throw new Win32Exception();
        IntPtr input = CreateFileW("CONIN$", 0xC0000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
        if (input == new IntPtr(-1)) throw new Win32Exception();
        try {
            InputRecord[] records = new InputRecord[text.Length];
            for (int i = 0; i < text.Length; i++) {
                records[i].EventType = KeyEventType;
                records[i].KeyEvent.KeyDown = 1;
                records[i].KeyEvent.RepeatCount = 1;
                records[i].KeyEvent.UnicodeChar = text[i];
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
if (-not $env:PI_CONSOLE_KEYS) { throw 'PI_CONSOLE_KEYS is not set' }
[PiConsoleKeys]::Type([uint32]$env:PI_CONSOLE_ATTACH_PID, $env:PI_CONSOLE_KEYS)
