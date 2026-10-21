# AirBridge application keep rules.
#
# The library publishes its own consumer rules; these cover app-side pieces
# that are only referenced through Compose reflection-free code paths and the
# local Binder.

-keep class com.airbridge.app.core.AirBridgeService$LocalBinder { *; }

# UniFFI/JNA: keep the generated surface and anything JNA reflects over.
-keep class com.airbridge.core.** { *; }
-keep class com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.** { public *; }
-dontwarn java.awt.**
