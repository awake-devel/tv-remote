package fr.ryoku.tvremote

import android.content.Intent
import android.content.pm.ActivityInfo
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.test.hasClickAction
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextClearance
import androidx.compose.ui.test.performTextInput
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.swipe
import androidx.compose.ui.test.click
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import fr.ryoku.tvremote.core.FoundTv
import fr.ryoku.tvremote.core.RemoteException
import fr.ryoku.tvremote.core.RemoteKey
import fr.ryoku.tvremote.core.TvPower
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class TouchpadTest : AppTestBase() {
    @Test
    fun dragMovesAndTapClicks() {
        launchWithTv()
        tab("Touchpad")
        val pad = rule.onNode(hasText("Drag to move", substring = true), useUnmergedTree = true)
        pad.performTouchInput { swipe(center, center + Offset(400f, 150f), durationMillis = 400) }
        waitFor { tv.calls.any { it is Call.Move } }
        val moves = tv.calls.filterIsInstance<Call.Move>()
        assertTrue("moves right", moves.sumOf { it.dx } > 0)
        assertTrue("moves down", moves.sumOf { it.dy } > 0)
        pad.performTouchInput { click(center) }
        waitFor { tv.has(Call.Click) }
        press("Back")
        waitFor { tv.keys().lastOrNull() == RemoteKey.RETURN }
        press("Home")
        waitFor { tv.keys().lastOrNull() == RemoteKey.HOME }
        press("Text")
        waitForText("Type on the TV")
    }
}

@RunWith(AndroidJUnit4::class)
class AppsTest : AppTestBase() {
    @Test
    fun listsAndLaunchesApps() {
        launchWithTv()
        tab("Apps")
        waitForText("Browser")
        // Sorted by name, tapping launches.
        clickText("Browser")
        waitFor { tv.has(Call.Launch("org.tizen.browser")) }
        // Shortcut row first, then the list entry with the same name.
        val youtube = rule.onAllNodes(hasText("YouTube") and hasClickAction())
        youtube[0].performClick()
        waitFor { tv.has(Call.LaunchId(MainViewModel.YOUTUBE_APP_ID)) }
        youtube[1].performClick()
        waitFor { tv.has(Call.Launch(MainViewModel.YOUTUBE_APP_ID)) }
        val netflix = rule.onAllNodes(hasText("Netflix") and hasClickAction())
        netflix[0].performClick()
        waitFor { tv.has(Call.Netflix) }
        netflix[1].performClick()
        waitFor { tv.has(Call.Launch("3201907018807")) }
    }

    @Test
    fun addAnAppById() {
        launchWithTv()
        tab("Apps")
        waitForText("Browser")
        waitForText("known app ids", substring = true)
        clickText("Missing an app? Add it by id")
        waitForText("Add an app by id")
        rule.onNode(hasSetTextAction()).performTextInput("3202311033030")
        clickText("Add")
        waitForText("Pathé Home")
        waitForText("Added Pathé Home")
        // Remembered: the next refresh probes it again.
        rule.onNode(hasContentDescription("Reload the list")).performClick()
        waitFor { tv.extraProbes.any { it.contains("3202311033030") } && tv.extraProbes.size >= 3 }
        // An id the TV doesn't have is refused.
        clickText("Missing an app? Add it by id")
        rule.onNode(hasSetTextAction()).performTextInput("3209999999999")
        clickText("Add")
        // After the "Added" snackbar (4 s) has gone.
        waitFor(15_000) { exists(hasText("no app with id 3209999999999", substring = true)) }
    }

    @Test
    fun listErrorOffersARetry() {
        launch {
            addTv(TV_NAME, "192.0.2.10").appsFailure = RemoteException.Unreachable("TV off")
        }
        waitForText(TV_NAME)
        tab("Apps")
        waitForText("Could not list the apps", substring = true)
        tv.appsFailure = null
        clickText("Try again")
        waitForText("Browser")
    }
}

@RunWith(AndroidJUnit4::class)
class TvsTest : AppTestBase() {
    private val bedroom = FoundTv("lan:192.0.2.9", "Bedroom TV", "QE55TEST", "192.0.2.9", "00:00:5e:00:53:09", TvPower.ON, null)

    @Test
    fun firstRunScanAddAndPair() {
        launch { found = listOf(bedroom) }
        waitForText("No TV yet", substring = true)
        clickText("Search the network")
        waitForText("Bedroom TV")
        clickText("Add")
        waitForText("Not paired", substring = true)
        assertEquals(1, core.tvs().size)
        clickText("Pair: a prompt appears on the TV")
        waitFor { tv.has(Call.Connect) }
    }

    @Test
    fun scanOffTheLanSaysSo() {
        launch()
        backend.lan = false
        waitForText("Your TVs")
        clickText("Search the network")
        waitForText("Connect the phone to the TV's Wi-Fi")
        assertEquals(0, core.scans)
    }

    @Test
    fun scanWithNothingFound() {
        launch()
        waitForText("Your TVs")
        clickText("Search the network")
        waitForText("No Samsung TV answered", substring = true)
    }

    @Test
    fun addByIpDialog() {
        launch()
        waitForText("Your TVs")
        clickText("By IP")
        waitForText("Add a TV by address")
        val fields = rule.onAllNodes(hasSetTextAction())
        fields[0].performTextInput("192.0.2.20")
        fields[1].performTextInput("Kitchen")
        clickText("Add")
        waitFor { core.addedHosts.contains("192.0.2.20" to "Kitchen") }
        waitForText("Kitchen")
    }

    @Test
    fun renameAndRemove() {
        launchWithTv()
        tab("TVs")
        rule.onNode(hasContentDescription("Rename")).performClick()
        waitForText("Save")
        rule.onNode(hasSetTextAction()).performTextClearance()
        rule.onNode(hasSetTextAction()).performTextInput("Salon")
        clickText("Save")
        waitFor { core.renamed.contains("tv1" to "Salon") }
        waitForText("Salon")
        rule.onNode(hasContentDescription("Remove")).performClick()
        waitForText("Remove Salon?")
        clickText("Remove")
        waitFor { core.removed.contains("tv1") }
        waitForText("No TV yet", substring = true)
    }

    @Test
    fun switchingTheActiveTv() {
        launch {
            addTv(TV_NAME, "192.0.2.10")
            addTv("Bedroom", "192.0.2.11")
        }
        waitForText(TV_NAME)
        tab("TVs")
        clickText("Bedroom")
        waitFor { core.activeId() == "tv2" }
        tab("Remote")
        waitForText("Bedroom")
        press("Home")
        waitFor { core.tvs.getValue("tv2").keys().contains(RemoteKey.HOME) }
        assertTrue(tv.keys().isEmpty())
    }
}

@RunWith(AndroidJUnit4::class)
class LifecycleTest : AppTestBase() {
    @Test
    fun rotationKeepsTheTabAndState() {
        launchWithTv()
        tab("Apps")
        waitForText("Browser")
        scenario.onActivity { it.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_LANDSCAPE }
        waitFor { scenario.state == Lifecycle.State.RESUMED }
        rule.waitForIdle()
        waitForText("Browser")
        scenario.recreate()
        waitForText("Browser")
        scenario.onActivity { it.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_PORTRAIT }
        waitForText("Browser")
    }

    @Test
    fun backgroundStopsPollingAndDisconnects() {
        launchWithTv()
        waitFor { core.polling && tv.has(Call.Connect) }
        scenario.moveToState(Lifecycle.State.CREATED)
        waitFor { !core.polling && tv.has(Call.Disconnect) }
        val connects = tv.calls.count { it == Call.Connect }
        scenario.moveToState(Lifecycle.State.RESUMED)
        waitFor { core.polling && tv.calls.count { it == Call.Connect } > connects }
    }

    @Test
    fun unpairedTvIsNotConnectedOnItsOwn() {
        launch { addTv(TV_NAME, "192.0.2.10", paired = false) }
        waitForText(TV_NAME)
        waitFor { core.polling }
        Thread.sleep(500)
        assertTrue(tv.calls.none { it == Call.Connect })
    }

    @Test
    fun sharedYouTubeLinkPlaysOnTheTv() {
        val intent = Intent(ApplicationProvider.getApplicationContext(), MainActivity::class.java)
            .setAction(Intent.ACTION_SEND)
            .setType("text/plain")
            .putExtra(Intent.EXTRA_TEXT, "Look: https://youtu.be/dQw4w9WgXcQ?si=x.")
        launch(intent) { addTv(TV_NAME, "192.0.2.10") }
        waitFor { tv.has(Call.YouTube("https://youtu.be/dQw4w9WgXcQ?si=x")) }
        waitForText("opened YouTube instead", substring = true)
    }

    @Test
    fun sharedTextWithoutALinkIsIgnored() {
        val intent = Intent(ApplicationProvider.getApplicationContext(), MainActivity::class.java)
            .setAction(Intent.ACTION_SEND)
            .setType("text/plain")
            .putExtra(Intent.EXTRA_TEXT, "no link here")
        launch(intent) { addTv(TV_NAME, "192.0.2.10") }
        waitForText(TV_NAME)
        Thread.sleep(500)
        assertTrue(tv.calls.none { it is Call.YouTube })
    }
}
