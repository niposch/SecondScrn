# UniFFI bindings use JNA, which reflects on these classes.
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-keep class uniffi.** { *; }
-dontwarn java.awt.**
# JNI entry point
-keep class dev.secondscrn.client.NativeSurface { *; }
