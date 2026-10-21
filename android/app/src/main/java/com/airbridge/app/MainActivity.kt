package com.airbridge.app

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import com.airbridge.app.ui.AirBridgeApp
import com.airbridge.app.ui.theme.AirBridgeTheme

class MainActivity : ComponentActivity() {
    private val viewModel: AirBridgeViewModel by viewModels()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
        )
        setContent {
            AirBridgeTheme {
                AirBridgeApp(viewModel = viewModel)
            }
        }
    }
}
