package com.airbridge.app.ui

import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.airbridge.app.ui.theme.Mint
import com.airbridge.app.ui.theme.PrimaryCyan
import com.airbridge.app.ui.theme.RadarBackground
import com.airbridge.app.ui.theme.RadarBackgroundDeep
import com.airbridge.app.ui.theme.TextPrimary
import com.airbridge.app.ui.theme.TextMuted

/** Full-screen gradient with the ambient cyan/mint blooms behind the radar. */
@Composable
fun AirBridgeBackground(modifier: Modifier = Modifier) {
    Canvas(modifier.fillMaxSize()) {
        drawRect(
            Brush.verticalGradient(
                colors = listOf(RadarBackground, RadarBackgroundDeep, RadarBackground),
            ),
        )
        drawCircle(
            brush = Brush.radialGradient(
                colors = listOf(PrimaryCyan.copy(alpha = 0.16f), Color.Transparent),
                center = Offset(size.width * 0.85f, size.height * 0.10f),
                radius = size.width * 0.85f,
            ),
            radius = size.width * 0.85f,
            center = Offset(size.width * 0.85f, size.height * 0.10f),
        )
        drawCircle(
            brush = Brush.radialGradient(
                colors = listOf(Mint.copy(alpha = 0.10f), Color.Transparent),
                center = Offset(size.width * 0.10f, size.height * 0.82f),
                radius = size.width * 0.75f,
            ),
            radius = size.width * 0.75f,
            center = Offset(size.width * 0.10f, size.height * 0.82f),
        )
    }
}

/** Frosted panel used for cards, pills and sheets. */
@Composable
fun GlassSurface(
    modifier: Modifier = Modifier,
    shape: Shape = RoundedCornerShape(22.dp),
    alpha: Float = 0.06f,
    borderAlpha: Float = 0.09f,
    content: @Composable BoxScope.() -> Unit,
) {
    Box(
        modifier = modifier
            .clip(shape)
            .background(Color.White.copy(alpha = alpha))
            .border(1.dp, Color.White.copy(alpha = borderAlpha), shape),
        content = content,
    )
}

@Composable
fun PrimaryActionButton(
    text: String,
    iconRes: Int,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
) {
    val shape = RoundedCornerShape(20.dp)
    Row(
        modifier = modifier
            .clip(shape)
            .background(
                if (enabled) {
                    Brush.horizontalGradient(listOf(Color(0xFF6FD6FF), Mint))
                } else {
                    Brush.horizontalGradient(
                        listOf(Color.White.copy(alpha = 0.10f), Color.White.copy(alpha = 0.10f)),
                    )
                },
            )
            .clickable(enabled = enabled, onClick = onClick)
            .padding(horizontal = 20.dp, vertical = 16.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.Center,
    ) {
        Icon(
            painter = painterResource(iconRes),
            contentDescription = null,
            modifier = Modifier.size(20.dp),
            tint = if (enabled) Color(0xFF04202A) else TextMuted,
        )
        Spacer(Modifier.width(10.dp))
        Text(
            text = text,
            color = if (enabled) Color(0xFF04202A) else TextMuted,
            fontSize = 15.sp,
            fontWeight = FontWeight.SemiBold,
        )
    }
}

@Composable
fun SecondaryActionButton(
    text: String,
    iconRes: Int,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
) {
    val shape = RoundedCornerShape(20.dp)
    Row(
        modifier = modifier
            .clip(shape)
            .background(Color.White.copy(alpha = if (enabled) 0.06f else 0.03f))
            .border(1.dp, Color.White.copy(alpha = 0.12f), shape)
            .clickable(enabled = enabled, onClick = onClick)
            .padding(horizontal = 20.dp, vertical = 16.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.Center,
    ) {
        Icon(
            painter = painterResource(iconRes),
            contentDescription = null,
            modifier = Modifier.size(18.dp),
            tint = if (enabled) PrimaryCyan else TextMuted,
        )
        Spacer(Modifier.width(10.dp))
        Text(
            text = text,
            color = if (enabled) TextPrimary else TextMuted,
            fontSize = 14.sp,
            fontWeight = FontWeight.Medium,
        )
    }
}

/** Small status pill used for the discovery tiers and labels. */
@Composable
fun StatusPill(
    label: String,
    dotColor: Color,
    modifier: Modifier = Modifier,
) {
    Row(
        modifier = modifier
            .clip(CircleShape)
            .background(Color.White.copy(alpha = 0.05f))
            .border(1.dp, Color.White.copy(alpha = 0.08f), CircleShape)
            .padding(horizontal = 10.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier
                .size(6.dp)
                .background(dotColor, CircleShape),
        )
        Spacer(Modifier.width(6.dp))
        Text(
            text = label,
            color = TextMuted,
            fontSize = 11.sp,
            fontWeight = FontWeight.Medium,
        )
    }
}

@Composable
fun PulseDot(color: Color, modifier: Modifier = Modifier, size: Dp = 7.dp) {
    val transition = rememberInfiniteTransition(label = "pulse")
    val alpha by transition.animateFloat(
        initialValue = 0.35f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(
            animation = tween(durationMillis = 1500),
            repeatMode = RepeatMode.Reverse,
        ),
        label = "pulse-alpha",
    )
    Box(
        modifier = modifier
            .size(size)
            .background(color.copy(alpha = alpha), CircleShape),
    )
}
