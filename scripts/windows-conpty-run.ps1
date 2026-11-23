# Runs the command line in $env:PI_CONPTY_COMMAND_LINE in a new ConPTY, exits with its code, and
# copies the pseudo console output to stderr. Used by crates/pi-tui/tests/windows_console.rs.
$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using Microsoft.Win32.SafeHandles;

public static class PiConPty {
    [StructLayout(LayoutKind.Sequential)]
    struct Coord { public short X; public short Y; }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct StartupInfo {
        public int cb;
        public string lpReserved, lpDesktop, lpTitle;
        public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public short wShowWindow, cbReserved2;
        public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct StartupInfoEx { public StartupInfo StartupInfo; public IntPtr lpAttributeList; }

    [StructLayout(LayoutKind.Sequential)]
    struct ProcessInformation { public IntPtr hProcess, hThread; public int dwProcessId, dwThreadId; }

    [DllImport("kernel32.dll")]
    static extern int CreatePseudoConsole(Coord size, SafeFileHandle input, SafeFileHandle output, uint flags, out IntPtr console);
    [DllImport("kernel32.dll")]
    static extern void ClosePseudoConsole(IntPtr console);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool CreatePipe(out SafeFileHandle read, out SafeFileHandle write, IntPtr attributes, int size);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool InitializeProcThreadAttributeList(IntPtr list, int count, int flags, ref IntPtr size);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool UpdateProcThreadAttribute(IntPtr list, uint flags, IntPtr attribute, IntPtr value, IntPtr size, IntPtr previous, IntPtr returnSize);
    [DllImport("kernel32.dll")]
    static extern void DeleteProcThreadAttributeList(IntPtr list);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateProcessW(string application, StringBuilder commandLine, IntPtr processAttributes, IntPtr threadAttributes, bool inheritHandles, uint flags, IntPtr environment, string directory, ref StartupInfoEx startupInfo, out ProcessInformation information);
    [DllImport("kernel32.dll")]
    static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetExitCodeProcess(IntPtr process, out uint code);
    [DllImport("kernel32.dll")]
    static extern bool TerminateProcess(IntPtr process, uint code);
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr handle);

    const uint ExtendedStartupInfoPresent = 0x00080000;
    const int UseStdHandles = 0x00000100;
    const int PseudoConsoleAttribute = 0x00020016;

    public static int Run(string commandLine, int timeoutMs) {
        SafeFileHandle inputRead, inputWrite, outputRead, outputWrite;
        if (!CreatePipe(out inputRead, out inputWrite, IntPtr.Zero, 0)) throw new Win32Exception();
        if (!CreatePipe(out outputRead, out outputWrite, IntPtr.Zero, 0)) throw new Win32Exception();
        IntPtr console;
        Coord size = new Coord { X = 120, Y = 30 };
        int result = CreatePseudoConsole(size, inputRead, outputWrite, 0, out console);
        if (result != 0) throw new Win32Exception(result, "CreatePseudoConsole failed: 0x" + result.ToString("x"));
        inputRead.Dispose();
        outputWrite.Dispose();

        MemoryStream output = new MemoryStream();
        Thread drain = new Thread(() => {
            using (FileStream stream = new FileStream(outputRead, FileAccess.Read)) {
                try { stream.CopyTo(output); } catch (IOException) { }
            }
        });
        drain.IsBackground = true;
        drain.Start();

        IntPtr listSize = IntPtr.Zero;
        InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref listSize);
        IntPtr list = Marshal.AllocHGlobal(listSize);
        try {
            if (!InitializeProcThreadAttributeList(list, 1, 0, ref listSize)) throw new Win32Exception();
            if (!UpdateProcThreadAttribute(list, 0, (IntPtr)PseudoConsoleAttribute, console, (IntPtr)IntPtr.Size, IntPtr.Zero, IntPtr.Zero)) {
                throw new Win32Exception();
            }
            StartupInfoEx startup = new StartupInfoEx();
            startup.StartupInfo.cb = Marshal.SizeOf(typeof(StartupInfoEx));
            // Null std handles keep this script's redirected pipes away from the child.
            startup.StartupInfo.dwFlags = UseStdHandles;
            startup.lpAttributeList = list;
            ProcessInformation process;
            if (!CreateProcessW(null, new StringBuilder(commandLine), IntPtr.Zero, IntPtr.Zero, false, ExtendedStartupInfoPresent, IntPtr.Zero, null, ref startup, out process)) {
                throw new Win32Exception();
            }
            uint code;
            if (WaitForSingleObject(process.hProcess, (uint)timeoutMs) != 0) {
                Console.Error.WriteLine("ConPTY child ran longer than " + timeoutMs + " ms");
                TerminateProcess(process.hProcess, 124);
                WaitForSingleObject(process.hProcess, 5000);
            }
            if (!GetExitCodeProcess(process.hProcess, out code)) throw new Win32Exception();
            CloseHandle(process.hThread);
            CloseHandle(process.hProcess);
            ClosePseudoConsole(console);
            drain.Join(5000);
            inputWrite.Dispose();
            Console.Error.WriteLine(Encoding.UTF8.GetString(output.ToArray()));
            return (int)code;
        } finally {
            DeleteProcThreadAttributeList(list);
            Marshal.FreeHGlobal(list);
        }
    }
}
'@

if (-not $env:PI_CONPTY_COMMAND_LINE) { throw 'PI_CONPTY_COMMAND_LINE is not set' }
exit [PiConPty]::Run($env:PI_CONPTY_COMMAND_LINE, 80000)
