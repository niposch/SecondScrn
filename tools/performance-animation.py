"""Animated test window on the tablet's monitor: a moving box at ~60 fps for a
fixed time, then exits. Usage: anim.py X Y SECONDS"""
import ctypes
import sys
import time
import tkinter as tk

ctypes.windll.shcore.SetProcessDpiAwareness(2)
ctypes.windll.winmm.timeBeginPeriod(1)
x, y, secs = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3])
root = tk.Tk()
root.title("SecondScrn test pattern")
root.geometry(f"1400x900+{x}+{y}")
root.attributes("-topmost", True)
c = tk.Canvas(root, width=1400, height=900, bg="#202020", highlightthickness=0)
c.pack()
box = c.create_rectangle(0, 300, 200, 500, fill="#3ba7ff", outline="")
label = c.create_text(700, 80, text="", fill="white", font=("Segoe UI", 28))
start = time.perf_counter()
frame = 0


def tick():
    global frame
    t = time.perf_counter() - start
    if t > secs:
        root.destroy()
        return
    frame += 1
    px = int((t * 900) % 1200)
    c.coords(box, px, 300, px + 200, 500)
    c.itemconfig(label, text=f"frame {frame}  t={t:5.2f}s")
    root.after(2, tick)


root.after(2, tick)
root.mainloop()
