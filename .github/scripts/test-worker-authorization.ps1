param([Parameter(Mandatory)][string]$WorkerExe)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'Service authorization acceptance requires a disposable GitHub-hosted runner.'
}
$root = Join-Path $env:ProgramData 'Gale\worker'
$configPath = Join-Path $root 'gale-worker.json'
$original = [IO.File]::ReadAllText($configPath)
$config = $original | ConvertFrom-Json
if ($config.profileId -ne 'ci-profile' -or $config.syncUrl -ne 'http://127.0.0.1:9') {
    throw 'The installed service is not the disposable CI fixture.'
}

Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Threading;
using Microsoft.Win32.SafeHandles;
public static class WorkerAuthorization {
    [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
    public static extern bool LogonUser(string user, string domain, string password, int type, int provider, out SafeAccessTokenHandle token);
    [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
    static extern IntPtr OpenSCManager(string machine, string database, uint access);
    [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
    static extern IntPtr OpenService(IntPtr manager, string name, uint access);
    [DllImport("advapi32.dll", SetLastError=true)]
    static extern bool CloseServiceHandle(IntPtr service);
    [StructLayout(LayoutKind.Sequential)]
    struct Status { public uint Type, State, Accepted, Exit, SpecificExit, Checkpoint, Wait; }
    [DllImport("advapi32.dll", SetLastError=true)]
    static extern bool ControlService(IntPtr service, uint control, out Status status);
    [DllImport("advapi32.dll", SetLastError=true)]
    static extern bool QueryServiceStatus(IntPtr service, out Status status);
    [DllImport("advapi32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
    static extern bool StartService(IntPtr service, uint count, IntPtr arguments);
    static void Check(bool success) { if (!success) throw new Win32Exception(Marshal.GetLastWin32Error()); }
    static IntPtr Manager() {
        var handle = OpenSCManager(null, null, 1);
        if (handle == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
        return handle;
    }
    public static int Open(uint access) {
        var manager = Manager();
        try {
            var service = OpenService(manager, "GaleWorker", access);
            if (service == IntPtr.Zero) return Marshal.GetLastWin32Error();
            CloseServiceHandle(service);
            return 0;
        } finally { CloseServiceHandle(manager); }
    }
    static void Wait(IntPtr service, uint expected) {
        for (int attempt=0; attempt<150; attempt++) {
            Status status;
            Check(QueryServiceStatus(service, out status));
            if (status.State == expected) return;
            Thread.Sleep(100);
        }
        throw new TimeoutException("Service did not reach the expected state.");
    }
    public static void Restart() {
        var manager = Manager();
        try {
            var service = OpenService(manager, "GaleWorker", 0x34);
            if (service == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
            try {
                Status status;
                Check(ControlService(service, 1, out status));
                Wait(service, 1);
                Check(StartService(service, 0, IntPtr.Zero));
                Wait(service, 4);
            } finally { CloseServiceHandle(service); }
        } finally { CloseServiceHandle(manager); }
    }
}
'@

function Reinstall-Fixture {
    & $WorkerExe service reinstall --log (Join-Path $env:RUNNER_TEMP 'authorization.log')
    if ($LASTEXITCODE -ne 0) { throw 'Fixture reinstall failed.' }
}
function Assert-Access($token, [bool]$control) {
    [Security.Principal.WindowsIdentity]::RunImpersonated($token, [Action]{
        if ([WorkerAuthorization]::Open(4) -ne 0) { throw 'Fixture account cannot query the service.' }
        if ($control) {
            [WorkerAuthorization]::Restart()
        } else {
            foreach ($access in @(0x14, 0x24)) {
                $result = [WorkerAuthorization]::Open($access)
                if ($result -ne 5) {
                    throw "Non-controller must be denied service access $access, received $result"
                }
            }
        }
    })
}

$accounts = @()
$tokens = @()
try {
    foreach ($name in @('GaleCiOwner', 'GaleCiOther')) {
        $password = 'Gale!' + [Guid]::NewGuid().ToString('N') + '9aA'
        $account = New-LocalUser -Name $name -Password (ConvertTo-SecureString $password -AsPlainText -Force)
        $accounts += $account
        if (@(Get-LocalGroupMember -SID 'S-1-5-32-545').SID.Value -notcontains $account.SID.Value) {
            Add-LocalGroupMember -SID 'S-1-5-32-545' -Member $account
        }
        $token = $null
        if (![WorkerAuthorization]::LogonUser($name, '.', $password, 2, 0, [ref]$token)) {
            throw [ComponentModel.Win32Exception]::new([Runtime.InteropServices.Marshal]::GetLastWin32Error())
        }
        $tokens += $token
        $password = $null
        $identity = [Security.Principal.WindowsIdentity]::new($token.DangerousGetHandle())
        try {
            if ($identity.Groups.Value -notcontains 'S-1-5-4' -or
                $identity.Groups.Value -contains 'S-1-5-32-544') {
                throw 'Fixture must use an interactive, non-administrator token.'
            }
        } finally { $identity.Dispose() }
    }
    $config | Add-Member -NotePropertyName controllerSid -NotePropertyValue $accounts[0].SID.Value -Force
    $config | ConvertTo-Json -Depth 10 | Set-Content -Encoding utf8NoBOM -LiteralPath $configPath
    Reinstall-Fixture
    Assert-Access $tokens[0] $true
    Assert-Access $tokens[1] $false
    $config.PSObject.Properties.Remove('controllerSid')
    $config | ConvertTo-Json -Depth 10 | Set-Content -Encoding utf8NoBOM -LiteralPath $configPath
    Reinstall-Fixture
    Assert-Access $tokens[0] $false
    Assert-Access $tokens[1] $false
} finally {
    try {
        [IO.File]::WriteAllText($configPath, $original)
        Reinstall-Fixture
    } finally {
        foreach ($token in $tokens) { $token.Dispose() }
        foreach ($account in $accounts) { Remove-LocalUser -SID $account.SID }
    }
}
