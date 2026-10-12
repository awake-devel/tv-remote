package fr.ryoku.tvremote

import android.app.Application
import androidx.compose.ui.test.hasClickAction
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import fr.ryoku.tvremote.logic.UiState
import fr.ryoku.tvremote.ui.TvsScreen
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

/** The Android 17 local network card, rendered with explicit state (no system dialog). */
@RunWith(AndroidJUnit4::class)
class PermissionCardTest {
    @get:Rule
    val rule = createComposeRule()

    @After
    fun tearDown() = Backends.reset()

    private fun vm(): MainViewModel {
        Backends.factory = { FakeBackend() }
        return MainViewModel(ApplicationProvider.getApplicationContext<Application>())
    }

    @Test
    fun cardShowsAndAsks() {
        val vm = vm()
        var asked = 0
        rule.setContent { TvsScreen(UiState(ready = true, lanAllowed = false), vm, onRequestLan = { asked++ }) }
        rule.onNodeWithText("Local network access is off").assertExists()
        rule.onNode(hasText("Allow") and hasClickAction()).performClick()
        assertEquals(1, asked)
    }

    @Test
    fun noCardWhenAllowed() {
        val vm = vm()
        rule.setContent { TvsScreen(UiState(ready = true, lanAllowed = true), vm, onRequestLan = {}) }
        rule.onNodeWithText("Local network access is off").assertDoesNotExist()
    }
}
