"""SQLAlchemy models."""
from sqlalchemy import Column, Integer, String
from sqlalchemy.orm import declarative_base

Base = declarative_base()


class Customer(Base):
    """Someone who places orders."""

    __tablename__ = "customers"

    id = Column(Integer, primary_key=True)
    email = Column(String, nullable=False)
