// SPDX-License-Identifier: GPL-2.0
//! Embed the icon and version resource into the Windows executable.

fn main() {
    println!("cargo:rerun-if-changed=../../icons/regulator.ico");
    #[cfg(target_os = "windows")]
    {
        let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../icons/regulator.ico");
        let mut res = winres::WindowsResource::new();
        res.set_icon(icon.to_str().unwrap());
        res.set("FileDescription", "regulator — watch timing machine");
        res.set("CompanyName", "regulator project");
        res.set("ProductName", "regulator");
        res.set("LegalCopyright", "GPLv2 \u{00a9} regulator contributors");
        res.set("OriginalFilename", "regulator-gui.exe");
        res.compile().unwrap();
    }
}
