fn main() {
    // Embed a Windows VERSIONINFO resource so the .exe's file metadata
    // (Explorer "Details" tab, PowerShell `Get-Command ... | Select Version`)
    // reports the crate version instead of 0.0.0.0. No-op on other platforms.
    //
    // `cfg(windows)` here is evaluated for the build host, which is correct for
    // native Windows builds (including the windows-latest release runner). A
    // Linux->Windows cross-compile would skip this, which is acceptable.
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        // FileVersion / ProductVersion default to CARGO_PKG_VERSION, so they
        // track the crate version automatically.
        res.set("ProductName", "mdfwob");
        res.set(
            "FileDescription",
            "Market-data downloader for IBKR and Databento",
        );
        res.set("LegalCopyright", "Copyright © 2026 imozo studio");
        res.compile()
            .expect("failed to embed Windows version resource");
    }
}
