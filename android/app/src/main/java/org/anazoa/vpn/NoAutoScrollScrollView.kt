package org.anazoa.vpn

import android.content.Context
import android.graphics.Rect
import android.util.AttributeSet
import android.view.View
import android.widget.ScrollView

// A selectable TextView is focusable, and every time its text changes,
// Android tries to bring the (reset-to-start) cursor position into view by
// calling requestChildRectangleOnScreen() up the parent chain — landing here
// and scrolling to the top of the log on every update, fighting the explicit
// scroll-to-bottom LogActivity performs via fullScroll() (a separate call
// path this override doesn't affect).
class NoAutoScrollScrollView @JvmOverloads constructor(
    context: Context,
    attrs: AttributeSet? = null,
) : ScrollView(context, attrs) {
    override fun requestChildRectangleOnScreen(child: View, rectangle: Rect, immediate: Boolean): Boolean = false
}
