# Consumer rules for apps that shrink this library.
#
# UniFFI loads the native library through a JNA dynamic proxy for the
# generated `UniffiLib` interface, and the callback vtable holds methods
# reflectively referenced by JNA. Keeping the small generated surface intact
# is cheap insurance against R8 renaming something the native side expects by
# checksum.
-keep class com.airbridge.core.** { *; }
