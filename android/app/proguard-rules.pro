# The native bridge calls back into these by name and signature, so they must survive
# shrinking and obfuscation.
-keep class com.hypedriven.hypeterm.NativeBridge { *; }
-keep interface com.hypedriven.hypeterm.NativeCallbacks { *; }
-keep class com.hypedriven.hypeterm.KeystoreSecureStore {
    public boolean put(java.lang.String, byte[]);
    public byte[] get(java.lang.String);
    public boolean remove(java.lang.String);
    public boolean contains(java.lang.String);
}
-keep class com.hypedriven.hypeterm.GlyphRasterizer {
    public byte[] rasterize(java.lang.String, boolean, boolean, int, float, float, float, float);
    public float[] measure(float, float);
}

# Anything implementing the callback interface is constructed reflectively from JNI.
-keep class * implements com.hypedriven.hypeterm.NativeCallbacks { *; }

# Keep line numbers so a release crash report is still readable; nothing here carries
# terminal contents (spec §15).
-keepattributes SourceFile,LineNumberTable
