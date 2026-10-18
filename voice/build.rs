fn main() {
    for fw in ["AppKit", "AVFoundation", "AVFAudio", "Speech", "QuartzCore", "ServiceManagement", "Foundation", "CoreGraphics", "CoreAudio", "AudioToolbox", "CoreFoundation"] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}
