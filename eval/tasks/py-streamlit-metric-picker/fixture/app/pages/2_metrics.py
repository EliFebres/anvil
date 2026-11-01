import streamlit as st

from metrics import METRICS

st.title("Metrics")
for name, series in METRICS.items():
    st.write(f"{name}: {series}")
