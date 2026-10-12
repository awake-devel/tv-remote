package fr.ryoku.tvremote

import fr.ryoku.tvremote.core.Connection
import fr.ryoku.tvremote.core.CoreEvent
import fr.ryoku.tvremote.core.TvPower
import fr.ryoku.tvremote.core.TvSummary
import fr.ryoku.tvremote.logic.PointerAccumulator
import fr.ryoku.tvremote.logic.UiState
import fr.ryoku.tvremote.logic.ipv4Broadcast
import fr.ryoku.tvremote.logic.on
import fr.ryoku.tvremote.logic.statusLine
import fr.ryoku.tvremote.logic.withTvs
import fr.ryoku.tvremote.logic.youTubeLink
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

/** Pure logic only: nothing here loads the Rust library. */
class LogicTest {
    private fun tv(id: String, active: Boolean = false, paired: Boolean = true) =
        TvSummary(id, "TV $id", "192.0.2.${id.last()}", null, null, paired, active)

    private val two = UiState(ready = true, tvs = listOf(tv("tv1", active = true), tv("tv2", paired = false)))

    @Test
    fun connectionAndKeyboardFollowTheActiveTv() {
        val s = two.on("tv1", CoreEvent.Connection(Connection.Connected))
        assertEquals(Connection.Connected, s.connection)
        assertSame(s, s.on("tv2", CoreEvent.Connection(Connection.Connecting)))

        assertTrue(s.on("tv1", CoreEvent.KeyboardShown).keyboardShown)
        assertFalse(s.on("tv2", CoreEvent.KeyboardShown).keyboardShown)
        assertFalse(s.on("tv1", CoreEvent.KeyboardShown).on("tv1", CoreEvent.KeyboardHidden).keyboardShown)
    }

    @Test
    fun powerIsKeptPerTv() {
        val s = two.on("tv2", CoreEvent.Power(TvPower.STANDBY)).on("tv1", CoreEvent.Power(TvPower.ON))
        assertEquals(TvPower.ON, s.power)
        assertEquals(TvPower.STANDBY, s.powers["tv2"])
        assertSame(s, s.on("", CoreEvent.Power(TvPower.OFF)))
    }

    @Test
    fun pairingMarksTheTv() {
        val s = two.on("tv2", CoreEvent.Paired)
        assertTrue(s.tvs.first { it.id == "tv2" }.paired)
    }

    @Test
    fun switchingTvResetsItsState() {
        val s = two.on("tv1", CoreEvent.Connection(Connection.Connected)).copy(volume = 12, muted = false)
        val same = s.withTvs(s.tvs.map { it.copy(name = "x") })
        assertEquals(12, same.volume)
        val other = s.withTvs(listOf(tv("tv1"), tv("tv2", active = true)))
        assertEquals("tv2", other.activeId)
        assertNull(other.volume)
        assertEquals(Connection.Disconnected, other.connection)
    }

    @Test
    fun statusLines() {
        assertEquals("No TV yet", statusLine(null, Connection.Disconnected, null))
        assertEquals("Accept the prompt on the TV", statusLine(tv("tv1", paired = false), Connection.Connecting, null))
        assertEquals("Connecting…", statusLine(tv("tv1"), Connection.Connecting, null))
        assertEquals("Connected", statusLine(tv("tv1"), Connection.Connected, TvPower.ON))
        assertEquals("TV off", statusLine(tv("tv1"), Connection.Retrying("refused"), TvPower.OFF))
        assertEquals("Off or unreachable", statusLine(tv("tv1"), Connection.Disconnected, TvPower.OFF))
    }

    @Test
    fun youTubeLinks() {
        assertEquals(
            "https://youtu.be/dQw4w9WgXcQ?si=abc",
            youTubeLink("Watch this: https://youtu.be/dQw4w9WgXcQ?si=abc."),
        )
        assertEquals(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            youTubeLink("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
        )
        assertNull(youTubeLink("https://example.com/youtube.com"))
        assertNull(youTubeLink("no link"))
    }

    @Test
    fun pointerKeepsRemainders() {
        val p = PointerAccumulator(gain = 1f, accel = 0f)
        p.add(0.4f, -0.4f)
        assertNull(p.take())
        p.add(0.7f, -0.7f)
        assertEquals(1 to -1, p.take())
        p.add(0.0f, 0.0f)
        assertNull(p.take()) // 0.1 left on each axis
        p.reset()
        p.add(2.5f, 0f)
        assertEquals(2 to 0, p.take())
    }

    @Test
    fun pointerAccelerates() {
        val slow = PointerAccumulator().apply { add(2f, 0f) }.take()!!.first
        val fast = PointerAccumulator().apply { add(40f, 0f) }.take()!!.first
        assertTrue(fast > 20 * slow)
    }

    @Test
    fun broadcasts() {
        val ip = byteArrayOf(192.toByte(), 168.toByte(), 0, 42)
        assertEquals("192.168.0.255", ipv4Broadcast(ip, 24))
        assertEquals("192.168.1.255", ipv4Broadcast(ip, 23))
        assertEquals("10.255.255.255", ipv4Broadcast(byteArrayOf(10, 1, 2, 3), 8))
        assertEquals("192.168.0.42", ipv4Broadcast(ip, 32))
        assertNull(ipv4Broadcast(ip, 33))
        assertNull(ipv4Broadcast(byteArrayOf(1, 2), 24))
    }
}
