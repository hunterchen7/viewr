#ifndef EVENTHANDLER_H
#define EVENTHANDLER_H

#include <QObject>
#include <QString>
#include <QLabel>

class EventHandler : public QObject {
    Q_OBJECT

public:
    explicit EventHandler(QObject *parent = nullptr);

    void handlePreviousImage(QLabel *label);
    void handleNextImage(QLabel *label);
    void handleRating(int rating, QLabel *label);

    // Future event-handling methods can be added here
};

#endif // EVENTHANDLER_H
