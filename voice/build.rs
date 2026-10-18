fn main() {
    for fw in ["AppKit", "AVFoundation", "AVFAudio", "Speech", "QuartzCore", "ServiceManagement", "Foundation", "CoreGraphics", "CoreAudio", "AudioToolbox"] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}
