package fr.ryoku.tvremote

import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.SemanticsMatcher
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.hasClickAction
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.test.performTextInput
import androidx.compose.ui.test.performTouchInput
import androidx.test.ext.junit.runners.AndroidJUnit4
import fr.ryoku.tvremote.core.Connection
import fr.ryoku.tvremote.core.CoreEvent
import fr.ryoku.tvremote.core.RemoteException
import fr.ryoku.tvremote.core.RemoteKey
import fr.ryoku.tvremote.core.TvPower
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class RemoteScreenTest : AppTestBase() {
    @Test
    fun showsTheActiveTvAndItsConnection() {
        launchWithTv()
        core.emit("tv1", CoreEvent.Connection(Connection.Connecting))
        waitForText("Connecting…")
        core.emit("tv1", CoreEvent.Connection(Connection.Connected))
        waitForText("Connected")
        core.emit("tv1", CoreEvent.Connection(Connection.Unauthorized))
        waitForText("Not allowed", substring = true)
    }

    @Test
    fun everyButtonSendsItsKey() {
        launchWithTv()
        val buttons = listOf(
            "Source" to RemoteKey.SOURCE,
            "Home" to RemoteKey.HOME,
            "Settings" to RemoteKey.MENU,
            "Up" to RemoteKey.UP,
            "Left" to RemoteKey.LEFT,
            "OK" to RemoteKey.ENTER,
            "Right" to RemoteKey.RIGHT,
            "Down" to RemoteKey.DOWN,
            "Back" to RemoteKey.RETURN,
            "Info" to RemoteKey.INFO,
            "Options" to RemoteKey.TOOLS,
            "Exit" to RemoteKey.EXIT,
            "VOL up" to RemoteKey.VOLUME_UP,
            "VOL down" to RemoteKey.VOLUME_DOWN,
            "CH up" to RemoteKey.CHANNEL_UP,
            "CH down" to RemoteKey.CHANNEL_DOWN,
            "Rewind" to RemoteKey.REWIND,
            "Play" to RemoteKey.PLAY,
            "Pause" to RemoteKey.PAUSE,
            "Stop" to RemoteKey.STOP,
            "Fast forward" to RemoteKey.FAST_FORWARD,
        )
        for ((desc, key) in buttons) {
            val before = tv.keys().size
            press(desc)
            waitFor { tv.keys().size > before }
            assertEquals("button $desc", key, tv.keys().last())
        }
        assertFalse("power key never sent", tv.keys().contains(RemoteKey.POWER))
    }

    @Test
    fun holdingARockerRepeats() {
        launchWithTv()
        rule.onNode(hasContentDescription("VOL up") and hasClickAction()).performTouchInput {
            down(center)
        }
        // The repeat starts after 400 ms, then every 130 ms. Advance both clocks: the
        // test's frame clock and real time.
        hold(1_200)
        rule.onNode(hasContentDescription("VOL up") and hasClickAction()).performTouchInput { up() }
        waitFor { tv.keys().count { it == RemoteKey.VOLUME_UP } >= 4 }
        val n = tv.keys().count { it == RemoteKey.VOLUME_UP }
        hold(600)
        assertEquals("repeat stops on release", n, tv.keys().count { it == RemoteKey.VOLUME_UP })
    }

    private fun hold(ms: Long) {
        repeat((ms / 50).toInt()) {
            rule.mainClock.advanceTimeBy(50)
            Thread.sleep(50)
        }
    }

    @Test
    fun muteTogglesTheTvMute() {
        launchWithTv()
        waitForText("Volume 10")
        press("Mute")
        waitFor { tv.has(Call.SetMuted(true)) }
        waitForText("Muted")
        press("Muted")
        waitFor { tv.has(Call.SetMuted(false)) }
    }

    @Test
    fun volumeSliderSetsTheVolume() {
        launchWithTv()
        waitForText("Volume 10")
        clickText("Volume 10")
        waitForText("Done")
        rule.onNode(SemanticsMatcher.keyIsDefined(SemanticsActions.SetProgress))
            .performSemanticsAction(SemanticsActions.SetProgress) { it(42f) }
        waitFor { tv.has(Call.SetVolume(42)) }
        clickText("Done")
        waitForText("Volume 42")
    }

    @Test
    fun powerButtonTurnsOffThenOnWithoutAPowerKey() {
        launchWithTv()
        waitFor { exists(hasContentDescription("Turn the TV off")) }
        press("Turn the TV off")
        waitFor { tv.has(Call.PowerOff) }
        waitFor { exists(hasContentDescription("Turn the TV on")) }
        press("Turn the TV on")
        waitFor { tv.calls.any { it is Call.PowerOn } }
        waitFor { exists(hasContentDescription("Turn the TV off")) }
        assertFalse(tv.keys().contains(RemoteKey.POWER))
        assertEquals(TvPower.ON, tv.power)
    }

    @Test
    fun numberPadSendsDigitsAndExtras() {
        launchWithTv()
        press("123")
        waitForText("Prev ch")
        val keys = (0..9).map { it.toString() to RemoteKey.valueOf("NUM$it") } + listOf(
            "Prev ch" to RemoteKey.PREVIOUS_CHANNEL,
            "Ch list" to RemoteKey.CHANNEL_LIST,
            "Guide" to RemoteKey.GUIDE,
            "Power off" to RemoteKey.POWER_OFF,
            "Red" to RemoteKey.RED,
            "Green" to RemoteKey.GREEN,
            "Yellow" to RemoteKey.YELLOW,
            "Blue" to RemoteKey.BLUE,
        )
        for ((desc, key) in keys) {
            val before = tv.keys().size
            press(desc)
            waitFor { tv.keys().size > before }
            assertEquals("pad $desc", key, tv.keys().last())
        }
    }

    @Test
    fun textButtonSendsTypedText() {
        launchWithTv()
        press("Text")
        waitForText("Type on the TV")
        rule.onNode(hasSetTextAction()).performTextInput("hello")
        clickText("Send")
        waitFor { tv.has(Call.Text("hello")) }
        waitGone("Type on the TV")
    }

    @Test
    fun tvKeyboardOpensTheTextDialog() {
        launchWithTv()
        core.emit("tv1", CoreEvent.KeyboardShown)
        waitForText("The TV asks for text")
        clickText("Cancel")
        waitGone("The TV asks for text")
        assertTrue(tv.calls.none { it is Call.Text })
    }

    @Test
    fun failuresShowASnackbar() {
        launchWithTv()
        tv.failure = RemoteException.Unreachable("connection refused")
        press("Home")
        waitForText("TV unreachable", substring = true)
    }

    @Test
    fun refusedPairingShowsASnackbar() {
        launchWithTv()
        tv.failure = RemoteException.NotAuthorized("denied")
        press("Home")
        waitForText("accept this phone on the TV", substring = true)
    }

    @Test
    fun withoutATvKeysAskToAddOne() {
        launch()
        // First run opens the TVs tab.
        waitForText("Your TVs")
        tab("Remote")
        waitForText("No TV yet")
        press("Home")
        waitForText("Add a TV first", substring = true)
    }
}
