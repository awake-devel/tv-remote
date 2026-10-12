package fr.ryoku.tvremote

import android.content.Intent
import android.content.pm.ActivityInfo
import androidx.compose.ui.test.SemanticsMatcher
import androidx.compose.ui.test.SemanticsNodeInteraction
import androidx.compose.ui.test.hasClickAction
import androidx.compose.ui.test.hasContentDescription
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.isSelectable
import androidx.compose.ui.test.junit4.v2.createEmptyComposeRule
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.test.core.app.ActivityScenario
import org.junit.After
import org.junit.Rule

/**
 * Runs the real activity over [FakeBackend]: no Rust core, no network, no TV. The view
 * model works on background threads, so every check waits for its condition (bounded).
 */
abstract class AppTestBase {
    @get:Rule
    val rule = createEmptyComposeRule()

    lateinit var backend: FakeBackend
    lateinit var scenario: ActivityScenario<MainActivity>

    val core: FakeCore get() = backend.core

    /** The first TV of the fake list. */
    val tv: FakeTv get() = core.tvs.getValue("tv1")

    fun launch(intent: Intent? = null, setup: FakeCore.() -> Unit = {}) {
        backend = FakeBackend()
        backend.core.setup()
        Backends.factory = { backend }
        Backends.lanGranted = { true }
        scenario = if (intent != null) ActivityScenario.launch(intent) else ActivityScenario.launch(MainActivity::class.java)
        waitFor { core.eventListener != null }
    }

    /** Launch with one paired TV, on the Remote tab, once it shows. */
    fun launchWithTv() {
        launch { addTv(TV_NAME, "192.0.2.10") }
        waitForText(TV_NAME)
    }

    @After
    fun tearDown() {
        if (::scenario.isInitialized) {
            scenario.onActivity { it.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED }
            scenario.close()
        }
        Backends.reset()
    }

    fun waitFor(timeoutMs: Long = 5_000, condition: () -> Boolean) {
        rule.waitUntil(timeoutMs) { condition() }
    }

    fun exists(matcher: SemanticsMatcher): Boolean =
        rule.onAllNodes(matcher, useUnmergedTree = true).fetchSemanticsNodes().isNotEmpty()

    fun waitForText(text: String, substring: Boolean = false) =
        waitFor { exists(hasText(text, substring = substring)) }

    fun waitGone(text: String, substring: Boolean = false) =
        waitFor { !exists(hasText(text, substring = substring)) }

    /** A remote key (its content description), scrolled into view and pressed. */
    fun press(description: String) {
        rule.onNode(hasContentDescription(description) and hasClickAction()).scrollIfPossible().performClick()
    }

    fun clickText(text: String) {
        rule.onNode(hasText(text) and hasClickAction()).scrollIfPossible().performClick()
    }

    fun tab(name: String) {
        rule.onNode(hasText(name) and isSelectable()).performClick()
        rule.waitForIdle()
    }

    private fun SemanticsNodeInteraction.scrollIfPossible(): SemanticsNodeInteraction =
        try {
            performScrollTo()
        } catch (_: AssertionError) {
            this
        }

    companion object {
        const val TV_NAME = "Living room"
    }
}
