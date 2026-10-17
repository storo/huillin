fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("ui/huillin.ico");
        res.set("ProductName", "Huillín");
        res.set("FileDescription", "Huillín - cliente BitTorrent");
        let _ = res.compile();
    }
}
