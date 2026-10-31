import streamlit as st

from metrics import latest, names

st.title("Metrics")
chosen = st.sidebar.selectbox("Metric", names())
st.metric(label=chosen, value=latest(chosen))
