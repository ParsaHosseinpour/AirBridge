package com.airbridge.app.ui.theme

import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Shapes
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

// AirDrop-inspired dark radar palette.
val RadarBackground = Color(0xFF04060B)
val RadarBackgroundDeep = Color(0xFF07131F)
val SurfaceGlass = Color(0xFF0C1523)
val PrimaryCyan = Color(0xFF5AC8FA)
val Mint = Color(0xFF6EE7C8)
val Amber = Color(0xFFFFCF70)
val Danger = Color(0xFFFF6B7A)
val TextPrimary = Color(0xFFEAF4FF)
val TextMuted = Color(0xFF9DB2C4)
val OutlineSoft = Color(0x1FFFFFFF)

private val AirBridgeScheme = darkColorScheme(
    primary = PrimaryCyan,
    onPrimary = Color(0xFF00202B),
    secondary = Mint,
    onSecondary = Color(0xFF00201A),
    background = RadarBackground,
    onBackground = TextPrimary,
    surface = SurfaceGlass,
    onSurface = TextPrimary,
    surfaceVariant = Color(0xFF16202F),
    onSurfaceVariant = TextMuted,
    outline = Color(0xFF2A3B52),
    error = Danger,
    onError = Color(0xFF33000A),
)

private val AirBridgeTypography = Typography(
    displaySmall = TextStyle(
        fontFamily = FontFamily.SansSerif,
        fontWeight = FontWeight.Light,
        fontSize = 32.sp,
    ),
    headlineSmall = TextStyle(
        fontFamily = FontFamily.SansSerif,
        fontWeight = FontWeight.SemiBold,
        fontSize = 22.sp,
    ),
    titleMedium = TextStyle(
        fontFamily = FontFamily.SansSerif,
        fontWeight = FontWeight.SemiBold,
        fontSize = 16.sp,
    ),
    bodyMedium = TextStyle(
        fontFamily = FontFamily.SansSerif,
        fontWeight = FontWeight.Normal,
        fontSize = 14.sp,
    ),
    bodySmall = TextStyle(
        fontFamily = FontFamily.SansSerif,
        fontWeight = FontWeight.Normal,
        fontSize = 12.sp,
    ),
    labelSmall = TextStyle(
        fontFamily = FontFamily.SansSerif,
        fontWeight = FontWeight.Medium,
        fontSize = 11.sp,
        letterSpacing = 0.4.sp,
    ),
)

private val AirBridgeShapes = Shapes(
    extraSmall = RoundedCornerShape(10.dp),
    small = RoundedCornerShape(14.dp),
    medium = RoundedCornerShape(20.dp),
    large = RoundedCornerShape(28.dp),
    extraLarge = RoundedCornerShape(34.dp),
)

@Composable
fun AirBridgeTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = AirBridgeScheme,
        typography = AirBridgeTypography,
        shapes = AirBridgeShapes,
        content = content,
    )
}
