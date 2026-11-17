// Top-level build file for the Mimint Android shell (P4.3).
// Versions are pinned: the shell must build reproducibly from a
// clean checkout. No third-party dependencies anywhere in this
// project — JSON via org.json, TLS via the platform, crypto via
// AndroidKeyStore — so there is no supply chain to audit.
plugins {
    id("com.android.application") version "8.5.2" apply false
    id("org.jetbrains.kotlin.android") version "2.0.20" apply false
}
