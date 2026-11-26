// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

using System.Net;
using System.Net.Http.Headers;
using System.Net.Http.Json;

namespace VmContextHelper;

public record ForegroundInfo(string AppName, string WindowTitle);

/// <summary>
/// Posts the latest foreground state to the host. Single-flight: one request at
/// a time, and a newer state replaces any pending one (latest wins). Holds no
/// state beyond the last value; nothing is persisted.
/// </summary>
public sealed class Sender
{
    const int MaxApp = 512, MaxTitle = 1024, HeartbeatMs = 2000;

    readonly Uri _baseUri;
    readonly Func<string> _readKey;
    readonly ulong _epoch;
    readonly Func<CancellationToken, Task<bool>>? _ready;
    readonly Action<string> _log;
    readonly HttpClient _http;
    readonly object _lock = new();
    readonly SemaphoreSlim _wake = new(0, 1);

    string? _key;
    long _seq;
    ForegroundInfo? _pending, _last;
    bool _hasPending, _paused, _blocked;
    string? _lastError;

    public Sender(Uri baseUri, Func<string> readKey, ulong epoch,
        Func<CancellationToken, Task<bool>>? ready = null,
        Action<string>? log = null, HttpMessageHandler? handler = null)
    {
        _baseUri = baseUri;
        _readKey = readKey;
        _epoch = epoch;
        _ready = ready;
        _log = log ?? (_ => { });
        // No system proxy and no redirects: the owner-PID gate only vouches for 127.0.0.1:3031.
        _http = new HttpClient(handler ?? new HttpClientHandler { UseProxy = false, AllowAutoRedirect = false });
        _http.Timeout = TimeSpan.FromSeconds(5);
    }

    /// <summary>null = clear. Replaces any pending state.</summary>
    public void Publish(ForegroundInfo? info)
    {
        lock (_lock) { _pending = info; _hasPending = true; _blocked = false; }
        try { _wake.Release(); } catch (SemaphoreFullException) { }
    }

    /// <summary>Stop heartbeats (session locked). An explicit Publish still sends.</summary>
    public void Pause() { lock (_lock) _paused = true; }
    public void Resume() { lock (_lock) _paused = false; }

    public object BuildBody(ForegroundInfo? info) => new
    {
        source = "parallels-vm",
        epoch = _epoch,
        seq = (ulong)Interlocked.Increment(ref _seq),
        app_name = Cap(info?.AppName ?? "", MaxApp),
        window_title = Cap(info?.WindowTitle ?? "", MaxTitle),
    };

    // UTF-16 units, never leaving half a surrogate pair; the server bound is in
    // chars (code points), which is <= units, so this always fits.
    static string Cap(string s, int max)
    {
        if (s.Length <= max) return s;
        int n = char.IsHighSurrogate(s[max - 1]) ? max - 1 : max;
        return s[..n];
    }

    const int NotReady = -1;

    /// <summary>
    /// Returns the final HTTP status, or -1 when the gate refused (nothing sent).
    /// Re-reads the key once on 403 and retries once.
    /// </summary>
    public async Task<int> SendAsync(ForegroundInfo? info, CancellationToken ct = default)
    {
        int status = await PostAsync(info, forceKeyRead: false, ct);
        if (status == 403)
            status = await PostAsync(info, forceKeyRead: true, ct);
        return status;
    }

    async Task<int> PostAsync(ForegroundInfo? info, bool forceKeyRead, CancellationToken ct)
    {
        // The gate runs before every keyed request, including the 403 retry:
        // the tunnel can drop and the port change hands during a round trip.
        if (_ready is not null && !await _ready(ct)) return NotReady;
        if (forceKeyRead || _key is null) _key = _readKey();
        var key = _key;
        using var req = new HttpRequestMessage(HttpMethod.Post, new Uri(_baseUri, "context/external"))
        {
            Content = JsonContent.Create(BuildBody(info)),
        };
        req.Headers.Authorization = new AuthenticationHeaderValue("Bearer", key);
        using var resp = await _http.SendAsync(req, ct);
        return (int)resp.StatusCode;
    }

    public async Task RunAsync(CancellationToken ct)
    {
        while (!ct.IsCancellationRequested)
        {
            try { await _wake.WaitAsync(HeartbeatMs, ct); }
            catch (OperationCanceledException) { return; }

            ForegroundInfo? info;
            lock (_lock)
            {
                if (_hasPending) { info = _last = _pending; _hasPending = false; }
                else if (!_paused && !_blocked && _last is not null) info = _last;
                else continue;
            }

            if (!await TrySendAsync(info, ct))
                lock (_lock) { if (!_hasPending) { _pending = info; _hasPending = true; } }
        }
    }

    // false = not delivered and worth retrying (tunnel not ready, network error).
    async Task<bool> TrySendAsync(ForegroundInfo? info, CancellationToken ct)
    {
        try
        {
            int status = await SendAsync(info, ct);
            if (status == NotReady) return false;
            if (status == 204) { _lastError = null; return true; }
            lock (_lock) _blocked = true; // 403 twice or 400: wait for the next state change
            LogError($"post rejected: HTTP {status}");
            return true;
        }
        catch (OperationCanceledException) when (ct.IsCancellationRequested) { return true; }
        catch (Exception e)
        {
            LogError($"post failed: {e.GetType().Name}"); // never e.Message: could echo request details
            return false;
        }
    }

    void LogError(string msg)
    {
        if (msg == _lastError) return;
        _lastError = msg;
        _log(msg);
    }
}
