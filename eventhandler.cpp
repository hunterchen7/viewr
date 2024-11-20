#include "eventhandler.h"
#include <QDebug>

EventHandler::EventHandler(QObject *parent) : QObject(parent) {}

void EventHandler::handlePreviousImage(QLabel *label) {
    qDebug() << "Previous image action triggered";
}

void EventHandler::handleNextImage(QLabel *label) {
    qDebug() << "Next image action triggered";
}

void EventHandler::handleRating(int rating, QLabel *label) {
    qDebug() << "Image rated with:" << rating;
}
