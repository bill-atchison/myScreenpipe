// screenpipe — AI that knows everything you've seen, said, or heard
// https://screenpipe.com

using System.Net;
using System.Text.Json;
using VmContextHelper;
using Xunit;

public class HelperTests
{
    static Sender NewSender(Func<string> key, HttpMessageHandler? h = null, ulong epoch = 42) =>
        new(new Uri("http://127.0.0.1:3031/"), key, epoch, handler: h);

    static JsonElement Body(Sender s, ForegroundInfo? info) =>
        JsonDocument.Parse(JsonSerializer.Serialize(s.BuildBody(info))).RootElement;

    [Fact]
    public void Normalize_StripsExe() =>
        Assert.Equal("MultiTerminal",
            Foreground.NormalizeAppName(@"C:\Program Files\MT\MultiTerminal.exe", null));

    [Fact]
    public void Normalize_UsesUwpName_ForApplicationFrameHost() =>
        Assert.Equal("CalculatorApp",
            Foreground.NormalizeAppName(@"C:\Windows\System32\ApplicationFrameHost.exe", "CalculatorApp"));

    [Fact]
    public void Normalize_UsesProductName_ForGenericStem() =>
        Assert.Equal("Warp",
            Foreground.NormalizeAppName(@"C:\Program Files\WarpPreview\preview.exe", null, "Warp"));

    [Fact]
    public void Normalize_KeepsStem_ForOrdinaryApp() =>
        Assert.Equal("chrome",
            Foreground.NormalizeAppName(@"C:\Program Files\Google\Chrome\Application\chrome.exe", null, "Google Chrome"));

    [Fact]
    public void Body_SeqStrictlyIncreases()
    {
        var s = NewSender(() => "k");
        var a = Body(s, new("App", "t"));
        var b = Body(s, new("App", "t"));
        Assert.Equal(1UL, a.GetProperty("seq").GetUInt64());
        Assert.Equal(2UL, b.GetProperty("seq").GetUInt64());
        Assert.Equal(42UL, a.GetProperty("epoch").GetUInt64());
        Assert.Equal(42UL, b.GetProperty("epoch").GetUInt64());
        Assert.Equal("parallels-vm", a.GetProperty("source").GetString());
    }

    [Fact]
    public void Body_NullIsClear()
    {
        var b = Body(NewSender(() => "k"), null);
        Assert.Equal("", b.GetProperty("app_name").GetString());
        Assert.Equal("", b.GetProperty("window_title").GetString());
    }

    [Fact]
    public void Body_TruncatesTitleTo1024()
    {
        var b = Body(NewSender(() => "k"), new("App", new string('x', 2000)));
        Assert.Equal(1024, b.GetProperty("window_title").GetString()!.Length);
    }

    [Fact]
    public void Body_TruncationNeverSplitsSurrogatePair()
    {
        // The emoji occupies UTF-16 units 1023 and 1024: a naive cut at 1024 keeps half of it.
        var title = new string('x', 1023) + "\U0001F600" + "y";
        // Read the raw string, not via JSON: the serializer would mask a lone surrogate as U+FFFD.
        var body = NewSender(() => "k").BuildBody(new("App", title));
        var t = (string)body.GetType().GetProperty("window_title")!.GetValue(body)!;
        Assert.True(t.Length <= 1024);
        Assert.False(char.IsHighSurrogate(t[^1]));
    }

    [Fact]
    public async Task Send_GateChecksBeforeEveryPost_NoRetryWhenNotReady()
    {
        var fake = new Fake(HttpStatusCode.Forbidden, HttpStatusCode.NoContent);
        int gateCalls = 0;
        var s = new Sender(new Uri("http://127.0.0.1:3031/"), () => "k", 42,
            ready: _ => Task.FromResult(++gateCalls == 1), handler: fake);
        var status = await s.SendAsync(new("App", "t"));
        Assert.Equal(-1, status);
        Assert.Equal(2, gateCalls);
        Assert.Single(fake.Auth);
    }

    sealed class Fake(params HttpStatusCode[] answers) : HttpMessageHandler
    {
        public List<string?> Auth = new();
        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage r, CancellationToken ct)
        {
            Auth.Add(r.Headers.Authorization?.Parameter);
            return Task.FromResult(new HttpResponseMessage(answers[Auth.Count - 1]));
        }
    }

    [Fact]
    public async Task Send_RereadsKeyOnceOn403()
    {
        var fake = new Fake(HttpStatusCode.Forbidden, HttpStatusCode.NoContent);
        int reads = 0;
        var s = NewSender(() => reads++ == 0 ? "old" : "new", fake);
        var status = await s.SendAsync(new("App", "t"));
        Assert.Equal(204, status);
        Assert.Equal(2, reads);
        Assert.Equal(new[] { "old", "new" }, fake.Auth);
    }
}
