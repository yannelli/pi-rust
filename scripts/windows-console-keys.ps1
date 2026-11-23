# Types $env:PI_CONSOLE_KEYS into the console of $env:PI_CONSOLE_ATTACH_PID as key records; a
# control character is Ctrl plus its letter. Start it detached so PowerShell never owns the console.
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
    const uint LeftCtrlPressed = 0x0008;

    static void Key(InputRecord[] records, int index, char ch, bool down) {
        ushort key = 0;
        uint state = 0;
        if (ch == '\r') { key = 0x0D; }
        else if (ch == '/') { key = 0xBF; }
        else if (ch >= 'a' && ch <= 'z') { key = (ushort)(ch - 'a' + 'A'); }
        else if (ch >= (char)1 && ch <= (char)26) { key = (ushort)(ch + 0x40); state = LeftCtrlPressed; }
        else { throw new ArgumentException("no key for U+" + ((int)ch).ToString("X4")); }
        records[index].EventType = KeyEventType;
        records[index].KeyEvent.KeyDown = down ? 1 : 0;
        records[index].KeyEvent.RepeatCount = 1;
        records[index].KeyEvent.VirtualKeyCode = key;
        records[index].KeyEvent.UnicodeChar = ch;
        records[index].KeyEvent.ControlKeyState = state;
    }

    public static void Type(uint processId, string text) {
        FreeConsole();
        if (!AttachConsole(processId)) throw new Win32Exception();
        IntPtr input = CreateFileW("CONIN$", 0xC0000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
        if (input == new IntPtr(-1)) throw new Win32Exception();
        try {
            InputRecord[] records = new InputRecord[text.Length * 2];
            for (int i = 0; i < text.Length; i++) {
                Key(records, 2 * i, text[i], true);
                Key(records, 2 * i + 1, text[i], false);
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
