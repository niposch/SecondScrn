package dev.secondscrn.client

import android.app.Application

class SecondScrnApp : Application() {
    /** One Rust client per process; it outlives activity recreation. */
    val repository: ClientRepository by lazy { ClientRepository(this) }
}
