fn main() {
    // Embed the application icon (Explorer / taskbar / Alt-Tab) via icon.rc
    // -> "1 ICON icon.ico". RC.EXE needs the .rc script, not the .ico itself.
    embed_resource::compile("icon.rc", embed_resource::NONE);
}
