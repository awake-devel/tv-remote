package fr.ryoku.tvremote.logic

import fr.ryoku.tvremote.core.AppInfo
import fr.ryoku.tvremote.core.Connection
import fr.ryoku.tvremote.core.CoreEvent
import fr.ryoku.tvremote.core.FoundTv
import fr.ryoku.tvremote.core.TvPower
import fr.ryoku.tvremote.core.TvSummary

/** Everything the screens show. Pure data: updated by [on] (core events) and the view model. */
data class UiState(
    /** False until the core is open (or [fatal] says why it could not be). */
    val ready: Boolean = false,
    val fatal: String? = null,
    val tvs: List<TvSummary> = emptyList(),
    val connection: Connection = Connection.Disconnected,
    /** Last known power per TV id. */
    val powers: Map<String, TvPower> = emptyMap(),
    val volume: Int? = null,
    val muted: Boolean? = null,
    val powerBusy: Boolean = false,
    val apps: List<AppInfo> = emptyList(),
    val appsLoading: Boolean = false,
    val appsError: String? = null,
    val scanning: Boolean = false,
    val found: List<FoundTv> = emptyList(),
    val scanError: String? = null,
    /** Whether a scan ran since the TVs screen was opened (to tell "none found" from "not scanned"). */
    val scanned: Boolean = false,
    /** The TV's on-screen keyboard is open: offer text entry. */
    val keyboardShown: Boolean = false,
    /** Android 17+: the local network permission is granted (always true before). */
    val lanAllowed: Boolean = true,
) {
    val active: TvSummary? get() = tvs.firstOrNull { it.active }
    val activeId: String? get() = active?.id
    val power: TvPower? get() = activeId?.let { powers[it] }
}

/**
 * Apply a core event. Events of a TV other than the active one only update its power and
 * pairing; connection and keyboard state follow the active TV.
 */
fun UiState.on(tvId: String, event: CoreEvent): UiState {
    val isActive = tvId.isNotEmpty() && tvId == activeId
    return when (event) {
        is CoreEvent.Connection ->
            if (isActive) copy(connection = event.state) else this
        is CoreEvent.Power ->
            if (tvId.isEmpty()) this else copy(powers = powers + (tvId to event.power))
        is CoreEvent.Paired ->
            copy(tvs = tvs.map { if (it.id == tvId) it.copy(paired = true) else it })
        is CoreEvent.KeyboardShown -> if (isActive) copy(keyboardShown = true) else this
        is CoreEvent.KeyboardHidden -> if (isActive) copy(keyboardShown = false) else this
        is CoreEvent.Remote, is CoreEvent.ConfigChanged, is CoreEvent.TvsChanged -> this
    }
}

/** Replace the TV list; per-TV state of another active TV is reset. */
fun UiState.withTvs(list: List<TvSummary>): UiState {
    val before = activeId
    val after = list.firstOrNull { it.active }?.id
    return if (before == after) {
        copy(tvs = list)
    } else {
        copy(
            tvs = list,
            connection = Connection.Disconnected,
            volume = null,
            muted = null,
            apps = emptyList(),
            appsError = null,
            keyboardShown = false,
        )
    }
}

/** One-line status for the active TV. */
fun statusLine(tv: TvSummary?, connection: Connection, power: TvPower?): String {
    if (tv == null) return "No TV yet"
    return when (connection) {
        is Connection.Connected -> if (power == TvPower.STANDBY) "Standby" else "Connected"
        is Connection.Connecting -> if (tv.paired) "Connecting…" else "Accept the prompt on the TV"
        is Connection.Unauthorized -> "Refused on the TV: allow it in the TV's device settings"
        is Connection.Retrying -> when (power) {
            TvPower.OFF -> "TV off"
            else -> "Reconnecting…"
        }
        is Connection.Disconnected -> when (power) {
            TvPower.ON -> if (tv.paired) "On" else "On, not paired yet"
            TvPower.STANDBY -> "Standby"
            TvPower.OFF -> "Off or unreachable"
            else -> if (tv.paired) "Not connected" else "Not paired yet"
        }
    }
}

/** The first YouTube link in shared text (`youtube.com/…`, `youtu.be/…`), or null. */
fun youTubeLink(text: String): String? =
    Regex("""https?://(?:www\.|m\.|music\.)?(?:youtube\.com|youtu\.be)/\S+""")
        .find(text)
        ?.value
        ?.trimEnd('.', ',', ')', ']', '"', '\'')
